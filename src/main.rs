#![deny(clippy::correctness)]
#![warn(
    clippy::suspicious,
    clippy::style,
    clippy::complexity,
    clippy::perf,
    missing_debug_implementations
)]

//! Aerostress - FTP load testing tool for stress testing data transfer.
//!
//! This utility creates test files and uploads them to an FTP server with configurable
//! parallelism, throttling, and batch delays. It's designed for benchmarking FTP
//! server performance under various load conditions.
//!
//! # Environment Variables
//! * `AEROSTRESS_SIZE` - Size of test file in megabytes (default: 10)
//! * `AEROSTRESS_TARGET` - FTP server address (default: 127.0.0.1)
//! * `AEROSTRESS_BATCHES` - Number of batches to send (default: 8)
//! * `AEROSTRESS_TASKS` - Parallel tasks per batch (default: 20)
//! * `AEROSTRESS_DELAY` - Delay between batches in seconds (default: 10)
//! * `AEROSTRESS_THROTTLE` - Upload throttle delay in ms (default: 0)
//! * `AEROSTRESS_CHUNK` - Chunk size for streaming in KB (default: 4)

const TEMP_FILE_NAME: &str = "mediumfile.dat";

mod config;
pub use config::Config;

use anyhow::{Context, Result};
use async_stream::stream;
use governor::{Quota, RateLimiter};
use log::{debug, error, info, warn};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    env,
    net::{SocketAddr, TcpStream as StdTcpStream},
    // num::NonZeroU32,
    pin::Pin,
    sync::Arc,
    time::Instant,
};
use suppaftp::{tokio::AsyncFtpStream, types::Mode};
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
    net::TcpStream,
    task::JoinSet,
    time::{Duration, sleep},
};
use tokio_stream::StreamExt;
use tokio_util::io::{ReaderStream, StreamReader};

#[derive(Debug, Copy, Clone)]
struct RateLimiterConfig {
    limiter: bool,
    size: u32,
    interval: u64,
    mss: u32,
}

impl RateLimiterConfig {
    /// Creates a new rate limiter configuration.
    fn new(limiter: bool, size: u32, interval: u64, mss: u32) -> Self {
        Self {
            limiter,
            size,
            interval,
            mss,
        }
    }
}

/// Creates a temporary file for testing with size from AEROSTRESS_SIZE env var.
///
/// # Returns
/// The actual file size in bytes.
///
/// # Errors
/// Returns an error if the file cannot be created, written, or flushed.
async fn setup_files() -> Result<u32> {
    let filesize = env::var("AEROSTRESS_SIZE").unwrap_or_else(|_| "10".to_string());
    let s: u32 = filesize
        .parse()
        .with_context(|| format!("AEROSTRESS_SIZE must be a valid number, got: {}", filesize))?;
    let target_size: u32 = s * 1024 * 1024;
    let mut current_size: u32 = 0;

    let file = File::create(TEMP_FILE_NAME)
        .await
        .context("Temporary file could not be created")?;
    let mut writer = BufWriter::new(file);
    // Using a buffer to speed up writing for large files
    const CHUNK_SIZE: u32 = 8192;
    let mut buffer = [0u8; CHUNK_SIZE as usize];

    while current_size < target_size {
        let remaining: u32 = target_size - current_size;
        let to_write: u32 = std::cmp::min(remaining, CHUNK_SIZE);

        rand::fill(&mut buffer[..to_write as usize]);

        writer
            .write_all(&buffer[..to_write as usize])
            .await
            .context("Chunk could not be written")?;
        current_size += to_write;
    }
    writer
        .flush()
        .await
        .context("Temporary file could not be flushed to disk")?;

    Ok(current_size)
}

/// Asynchronously uploads a file to an FTP server with optional throttling.
///
/// # Arguments
/// * `batch` - Batch identifier for logging
/// * `num` - Task number within batch  
/// * `filename` - Remote filename on FTP server
/// * `destination` - FTP server address:port
/// * `brake` - Throttle delay in milliseconds (0 = no throttling)
/// * `chunk` - Chunk size for streaming (KB)
///
/// # Returns
/// Number of bytes written to the FTP server.
///
/// # Errors
/// Returns an error if FTP connection, login, file upload, or stream finalization fails.
async fn write_async(
    batch: i32,
    num: i32,
    filename: &str,
    destination: &str,
    rlc: RateLimiterConfig,
) -> Result<u64> {
    let mut ftp_stream = AsyncFtpStream::connect(destination)
        .await
        .with_context(|| format!("FTP Stream {}-{} could not connect to server", batch, num))?;
    println!("Stream {}-{} connected to FTP server", batch, num);
    ftp_stream.set_mode(Mode::ExtendedPassive);
    ftp_stream
        .login("test", "secret")
        .await
        .with_context(|| format!("Login of Stream {}-{} to the FTP server failed", batch, num))?;
    debug!("Stream {}-{} logged in successfully", batch, num);

    let mut file = File::open(TEMP_FILE_NAME)
        .await
        .with_context(|| format!("Source file {}-{} could not be opened", batch, num))?;
    debug!("Stream {}-{} opened source file", batch, num);

    let bytes_written: u64;
    if rlc.limiter {
        ftp_stream = ftp_stream.passive_stream_builder(move |addr: SocketAddr| {
            // extract the one variable we need, satisfying the 'static lifetime requirement
            // of the async closure
            let mss = rlc.mss;
            let fut = async move {
                let socket =
                    Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                if mss > 0 {
                    socket
                        .set_tcp_mss(mss)
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                }
                socket
                    .connect(&addr.into())
                    .map_err(suppaftp::FtpError::ConnectionError)?;
                let std_stream: StdTcpStream = socket.into();
                std_stream
                    .set_nonblocking(true)
                    .map_err(suppaftp::FtpError::ConnectionError)?;
                TcpStream::from_std(std_stream).map_err(suppaftp::FtpError::ConnectionError)
            };
            Box::pin(fut)
                as Pin<
                    Box<
                        dyn futures::Future<
                                Output = Result<tokio::net::TcpStream, suppaftp::FtpError>,
                            > + Send
                            + Sync,
                    >,
                >
        });
        let mut data_stream = ftp_stream.put_with_stream(filename).await?;

        let mut reader_stream;
        if rlc.size > 0 {
            reader_stream = ReaderStream::with_capacity(file, (rlc.size) as usize);
        } else {
            reader_stream = ReaderStream::new(file);
        }

        if rlc.interval > 0 {
            // if rlc.interval > 0 {
            //     let throttled_reader = reader_stream.throttle(Duration::from_millis(rlc.interval));
            // }
            // let async_reader = StreamReader::new(throttled_reader);
            let quota = Quota::with_period(Duration::from_millis(rlc.interval)).unwrap();
            let limiter = Arc::new(RateLimiter::direct(quota));
            let throttled_reader = stream! {
                while let Some(chunk) = reader_stream.next().await {
                    limiter.until_ready().await;
                    yield chunk;
                }
            };
            let async_reader = StreamReader::new(throttled_reader);
            tokio::pin!(async_reader);
            println!(
                "Stream {}-{} created rate limited stream: interval {}, chunk size {}, mss {}",
                batch, num, rlc.interval, rlc.size, rlc.mss
            );

            bytes_written = tokio::io::copy(&mut async_reader, &mut data_stream)
                .await
                .with_context(|| format!("File {}-{} could not be streamed", batch, num))?;
            ftp_stream
                .finalize_put_stream(data_stream)
                .await
                .with_context(|| format!("File {}-{} could not be finalized", batch, num))?;
        } else {
            let async_reader = StreamReader::new(reader_stream);
            tokio::pin!(async_reader);
            println!(
                "Stream {}-{} created stream: interval {}, chunk size {}, mss {}",
                batch, num, rlc.interval, rlc.size, rlc.mss
            );
            bytes_written = tokio::io::copy(&mut async_reader, &mut data_stream)
                .await
                .with_context(|| format!("File {}-{} could not be streamed", batch, num))?;
            ftp_stream
                .finalize_put_stream(data_stream)
                .await
                .with_context(|| format!("File {}-{} could not be finalized", batch, num))?;
        }
    } else {
        bytes_written = ftp_stream
            .put_file(filename, &mut file)
            .await
            .with_context(|| format!("File {}-{} could not be sent", batch, num))?;
    }
    debug!("Stream {}-{} successfully wrote {}", batch, num, filename);
    ftp_stream
        .quit()
        .await
        .with_context(|| format!("Stream {}-{} failed to quit", batch, num))?;
    Ok(bytes_written)
}

// fn write_sync() {
//     // Create a connection to an FTP server and authenticate to it.
//     let mut ftp_stream = FtpStream::connect("127.0.0.1:2121").unwrap();
//     ftp_stream.login("rdiagftp", "siemens").unwrap();

//     // Store (PUT) a file from the client to the current working directory of the server.
//     let mut reader = Cursor::new("Hello from the Rust \"ftp\" crate!".as_bytes());
//     let _ = ftp_stream.put_file("greeting.txt", &mut reader);
//     println!("Successfully wrote greeting.txt");

//     // Terminate the connection to the server.
//     let _ = ftp_stream.quit();
// }

/// Handles errors from individual write tasks.
#[cold]
fn handle_task_error(e: &anyhow::Error) {
    error!("A write task failed: {:?}", e);
}

/// Handles errors from JoinHandle failures.
#[cold]
fn handle_join_error(e: &tokio::task::JoinError) {
    error!("A JoinHandle failed: {:?}", e);
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    info!("Creating temporary file to send");
    let file_size = setup_files().await?;
    info!("File created, {} bytes", file_size);

    let config = config::parse_config()?;
    let rlc = RateLimiterConfig::new(config.limiter, config.chunk, config.interval, config.mss);
    let destination = Arc::new(format!("{}:21", config.target));

    let start_time = Instant::now();
    let mut set: JoinSet<Result<u64>> = JoinSet::new();
    let mut error_count: u64 = 0;

    for j in 1..=config.batches {
        info!("Starting {} parallel tasks...", config.tasks);
        for i in 1..=config.tasks {
            let task_delay: f32 = rand::random::<f32>() * 0.75;
            let destination = Arc::clone(&destination);

            set.spawn(async move {
                sleep(Duration::from_secs(task_delay as u64)).await;
                let f = format!("testfile_{:02}_{:04}.txt", j, i);

                let start_time = Instant::now();
                let bytes_written = write_async(j, i, &f, &destination, rlc).await?;
                let elapsed = start_time.elapsed();
                info!(
                    "Task {} finished, {:.3} MiBytes, {:.3} kibit/s",
                    i,
                    bytes_written / 1024 / 1024,
                    (bytes_written * 8) as u128 / elapsed.as_millis(),
                );
                Ok(bytes_written)
            });
        }
        debug!(
            "Batch {} spawned {:?} seconds after start",
            j,
            start_time.elapsed()
        );
        sleep(Duration::from_secs(config.delay)).await;
    }

    let mut sum_bytes = 0u64;

    while let Some(res) = set.join_next().await {
        match res {
            Ok(taskresult) => match taskresult {
                Ok(b) => sum_bytes += b,
                Err(e) => {
                    handle_task_error(&e);
                    error_count += 1;
                }
            },
            Err(e) => {
                handle_join_error(&e);
                error_count += 1;
            }
        }
    }

    info!(
        "All tasks joined. Total elapsed time: {:?}, total GiB: {:?}, errors: {}",
        start_time.elapsed(),
        sum_bytes / 1024 / 1024 / 1024,
        error_count
    );

    Ok(())
}
