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
//! * `AEROSTRESS_TARGET`   - FTP server address (default: 127.0.0.1)
//! * `AEROSTRESS_SIZE`     - Size of test file in megabytes (default: 10)
//! * `AEROSTRESS_BATCHES`  - Number of batches to send (default: 8)
//! * `AEROSTRESS_TASKS`    - Parallel tasks per batch (default: 20)
//! * `AEROSTRESS_DELAY`    - Delay between batches in seconds (default: 10)
//! * `AEROSTRESS_LIMITER`  - Enable rate limiting: true/false (default: false)
//! * `AEROSTRESS_CHUNK`    - Chunk size for streaming in KB, required if limiter is enabled (default: 4)
//! * `AEROSTRESS_INTERVAL` - Rate limit interval in milliseconds, required if limiter is enabled (default: 0)
//! * `AEROSTRESS_MSS`      - TCP Maximum Segment Size in bytes (default: 1460)

const TEMP_FILE_NAME: &str = "mediumfile.dat";

mod config;
pub(crate) use config::{Config, parse_config};

use anyhow::{Context, Result};
use async_stream::stream;
use governor::{Quota, RateLimiter};
use log::{debug, error, info, warn};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    net::{SocketAddr, TcpStream as StdTcpStream},
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
    time::{Duration, sleep, timeout},
};
use tokio_stream::StreamExt;
use tokio_util::{
    io::{ReaderStream, StreamReader},
    sync::CancellationToken,
};

type TcpStreamFuture =
    Pin<Box<dyn futures::Future<Output = Result<TcpStream, suppaftp::FtpError>> + Send + Sync>>;

/// Configuration for rate limiting FTP uploads.
///
/// This struct controls bandwidth limiting behavior during file transfers, allowing you to:
/// - Simulate network constraints by throttling upload speeds
/// - Comply with transfer quotas or fairness policies  
/// - Test server performance under controlled load conditions
#[derive(Debug, Copy, Clone)]
pub(crate) struct RateLimiterConfig {
    /// Whether rate limiting is enabled for upload throttling
    pub(crate) limiter: bool,

    /// Chunk size for streaming in kilobytes (only used when limiter is enabled)
    pub(crate) size: u32,

    /// Rate limit interval in milliseconds; 0 disables throttling
    pub(crate) interval: u64,

    /// TCP Maximum Segment Size (MSS) for socket configuration; 0 uses system default
    pub(crate) mss: u32,
}

impl RateLimiterConfig {
    /// Creates a new rate limiter configuration.
    ///
    /// # Arguments
    /// * `limiter` - Enable or disable rate limiting
    /// * `size` - Chunk size for streaming in KB (must be > 0 if limiter is enabled)
    /// * `interval` - Rate limit interval in milliseconds; 0 disables throttling
    /// * `mss` - TCP Maximum Segment Size; set to 0 to use system default MSS
    ///
    /// # Examples
    /// ```no_run
    /// let rlc = RateLimiterConfig::new(true, 4, 100, 1460);
    /// // rate limiting enabled: 100ms interval, 4KB chunks, MSS=1460
    /// ```
    pub(crate) fn new(limiter: bool, size: u32, interval: u64, mss: u32) -> Self {
        Self {
            limiter,
            size,
            interval,
            mss,
        }
    }
}

/// Creates a temporary file for testing.
///
/// # Arguments
/// * `file_size_mb` - Size in megabytes to create
///
/// # Returns
/// The actual file size in bytes.
///
/// # Errors
/// Returns an error if the file cannot be created, written, or flushed.
async fn setup_files(file_size_mb: u32) -> Result<u64> {
    const BUFFER_CAPACITY: usize = 256 * 1024;

    let target_size: u64 = (file_size_mb as u64) * 1024 * 1024;
    let mut written: u64 = 0;

    let file = File::create(TEMP_FILE_NAME)
        .await
        .context("temporary file could not be created")?;
    let mut writer = BufWriter::with_capacity(BUFFER_CAPACITY, file);
    // Using a buffer to speed up writing for large files
    const CHUNK_SIZE: usize = 8192;
    let mut buffer = [0u8; CHUNK_SIZE];

    while written < target_size {
        let remaining: u64 = target_size - written;
        let to_write: usize = std::cmp::min(remaining as usize, CHUNK_SIZE);

        rand::fill(&mut buffer[..to_write]);

        writer
            .write_all(&buffer[..to_write])
            .await
            .context("chunk could not be written")?;
        written += to_write as u64;
    }
    writer
        .flush()
        .await
        .context("temporary file could not be flushed to disk")?;

    Ok(written)
}

/// Asynchronously uploads a file to an FTP server with optional rate limiting.
///
/// # Arguments
/// * `batch` - Batch identifier for logging
/// * `num` - Task number within the batch
/// * `filename` - Remote filename to create on the FTP server
/// * `destination` - FTP server address in `host:port` format
/// * `rlc` - Rate limiter configuration controlling chunk size, interval, and MSS
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
    debug!("Stream {}-{} connected to FTP server", batch, num);
    ftp_stream.set_mode(Mode::ExtendedPassive);
    ftp_stream
        .login("test", "secret")
        .await
        .with_context(|| format!("login of stream {}-{} to the FTP server failed", batch, num))?;
    debug!("Stream {}-{} logged in successfully", batch, num);

    let mut file = File::open(TEMP_FILE_NAME)
        .await
        .with_context(|| format!("source file {}-{} could not be opened", batch, num))?;
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
            Box::pin(fut) as TcpStreamFuture
        });
        let mut data_stream = ftp_stream.put_with_stream(filename).await?;

        let mut reader_stream;
        if rlc.size > 0 {
            reader_stream = ReaderStream::with_capacity(file, (rlc.size) as usize);
        } else {
            reader_stream = ReaderStream::new(file);
        }

        if rlc.interval > 0 {
            // Create rate limiter with proper error handling
            let quota = Quota::with_period(Duration::from_millis(rlc.interval))
                .context("rate limiter period could not be created (invalid interval)")?;
            let limiter = Arc::new(RateLimiter::direct(quota));
            let throttled_reader = stream! {
                while let Some(chunk) = reader_stream.next().await {
                    limiter.until_ready().await;
                    yield chunk;
                }
            };
            let async_reader = StreamReader::new(throttled_reader);
            tokio::pin!(async_reader);
            debug!(
                "Stream {}-{} created rate limited stream: interval {}, chunk size {}, mss {}",
                batch, num, rlc.interval, rlc.size, rlc.mss
            );

            bytes_written = tokio::io::copy(&mut async_reader, &mut data_stream)
                .await
                .with_context(|| format!("file {}-{} could not be streamed", batch, num))?;
            ftp_stream
                .finalize_put_stream(data_stream)
                .await
                .with_context(|| format!("file {}-{} could not be finalized", batch, num))?;
        } else {
            let async_reader = StreamReader::new(reader_stream);
            tokio::pin!(async_reader);
            debug!(
                "Stream {}-{} created stream: interval {}, chunk size {}, mss {}",
                batch, num, rlc.interval, rlc.size, rlc.mss
            );
            bytes_written = tokio::io::copy(&mut async_reader, &mut data_stream)
                .await
                .with_context(|| format!("file {}-{} could not be streamed", batch, num))?;
            ftp_stream
                .finalize_put_stream(data_stream)
                .await
                .with_context(|| format!("file {}-{} could not be finalized", batch, num))?;
        }
    } else {
        bytes_written = ftp_stream
            .put_file(filename, &mut file)
            .await
            .with_context(|| format!("file {}-{} could not be sent", batch, num))?;
    }
    info!("Stream {}-{} successfully wrote {}", batch, num, filename);
    ftp_stream
        .quit()
        .await
        .with_context(|| format!("stream {}-{} failed to quit", batch, num))?;
    Ok(bytes_written)
}

// fn write_sync() {
//     // Create a connection to an FTP server and authenticate to it.
//     let mut ftp_stream = FtpStream::connect("127.0.0.1:2121").unwrap();
//     ftp_stream.login("rdiagftp", "siemens").unwrap();

//     // Store (PUT) a file from the client to the current working directory of the server.
//     let mut reader = Cursor::new("Hello from the Rust \"ftp\" crate!".as_bytes());
//     let _ = ftp_stream.put_file("greeting.txt", &mut reader);
//     info!("Successfully wrote greeting.txt");

//     // Terminate the connection to the server.
//     let _ = ftp_stream.quit();
// }

/// Handles errors from individual write tasks during FTP upload.
///
/// This function logs task-specific errors for debugging and monitoring purposes.
/// It is annotated with `#[cold]` to optimize the compiler's code layout,
/// as error paths are rare compared to successful operations.
///
/// # Arguments
/// * `e` - The error from a failed write task
///
/// # Panics
/// This function does not panic; errors are logged for later investigation.
#[cold]
fn handle_task_error(e: &anyhow::Error) {
    error!("A write task failed: {:?}", e);
}

/// Handles failures from tokio task JoinHandle operations.
///
/// This function logs when a spawned task is cancelled, panicked, or otherwise fails to complete.
/// It is annotated with `#[cold]` to optimize the compiler's code layout,
/// as join failures are rare compared to successful task completion.
///
/// # Arguments
/// * `e` - The error from a failed JoinHandle operation
///
/// # Panics
/// This function does not panic; errors are logged for later investigation.
#[cold]
fn handle_join_error(e: &tokio::task::JoinError) {
    error!("A JoinHandle failed: {:?}", e);
}

/// Aggregated statistics from a completed run.
#[derive(Debug)]
struct RunResult {
    /// Number of tasks that completed successfully
    success_count: u64,
    /// Number of tasks that failed or were aborted
    error_count: u64,
    /// Total bytes transferred across all successful tasks
    sum_bytes: u64,
}

/// Parses configuration from environment variables and creates the test file.
///
/// # Errors
/// Returns an error if configuration is invalid or the test file cannot be created.
async fn prepare() -> Result<Config> {
    info!("Creating temporary file to send");
    let config = parse_config()?;
    let file_size_bytes = setup_files(config.file_size_mb).await?;
    info!("File created, {} bytes", file_size_bytes);
    Ok(config)
}

/// Installs SIGINT and SIGTERM handlers and returns a token that is
/// cancelled when either signal is received.
///
/// # Panics
/// Panics if the OS signal handlers cannot be registered.
fn install_shutdown_handler() -> CancellationToken {
    let shutdown = CancellationToken::new();
    let shutdown_trigger = shutdown.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        warn!("shutdown signal received, stopping batch spawning");
        shutdown_trigger.cancel();
    });
    shutdown
}

/// Spawns all configured batches of upload tasks and collects their results.
///
/// Batch spawning stops early if the shutdown token is cancelled. The result
/// collection loop also responds to a second cancellation by aborting any
/// remaining in-flight tasks immediately.
///
/// # Errors
/// Returns an error if the overall failure count is greater than zero.
async fn run(
    config: &Config,
    rlc: RateLimiterConfig,
    shutdown: &CancellationToken,
) -> Result<RunResult> {
    let destination = Arc::new(format!("{}:21", config.target));
    let mut set: JoinSet<Result<u64>> = JoinSet::new();
    let start_time = Instant::now();

    'batches: for j in 1..=config.batches {
        info!("Starting {} parallel tasks...", config.tasks);
        for i in 1..=config.tasks {
            let task_delay: f32 = rand::random::<f32>() * 0.75;
            let destination = Arc::clone(&destination);
            set.spawn(async move {
                sleep(Duration::from_secs_f32(task_delay)).await;
                let f = format!("testfile_{:02}_{:04}.txt", j, i);
                let task_start = Instant::now();
                let bytes_written = write_async(j, i, &f, &destination, rlc).await?;
                let elapsed = task_start.elapsed();
                info!(
                    "Task {} finished, {:.3} MiBytes, {:.1} kibit/s",
                    i,
                    bytes_written as f64 / 1024.0 / 1024.0,
                    (bytes_written * 8) as f64 / elapsed.as_millis().max(1) as f64,
                );
                Ok(bytes_written)
            });
        }
        debug!("Batch {} spawned {:?} after start", j, start_time.elapsed());

        // Inter-batch delay, interrupted immediately on shutdown signal
        tokio::select! {
            _ = sleep(Duration::from_secs(config.delay)) => {}
            _ = shutdown.cancelled() => {
                info!("shutdown signal received, stopping after batch {}", j);
                break 'batches;
            }
        }
    }

    let mut success_count: u64 = 0;
    let mut error_count: u64 = 0;
    let mut sum_bytes: u64 = 0;

    // Timeout for waiting on async task results (30 minutes).
    // This prevents the application from hanging indefinitely if tasks fail to complete.
    const TASK_JOIN_TIMEOUT_SECS: u64 = 1800;

    // Drain in-flight tasks; abort immediately on a second signal
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                warn!("shutdown signal received, aborting {} remaining task(s)", set.len());
                set.shutdown().await;
                break;
            }
            result = timeout(Duration::from_secs(TASK_JOIN_TIMEOUT_SECS), set.join_next()) => {
                match result {
                    Ok(Some(res)) => match res {
                        Ok(Ok(bytes)) => { sum_bytes += bytes; success_count += 1; }
                        Ok(Err(e))    => { handle_task_error(&e); error_count += 1; }
                        Err(e)        => { handle_join_error(&e); error_count += 1; }
                    },
                    Ok(None) => break,
                    Err(_) => {
                        warn!("timeout waiting for tasks after {} seconds", TASK_JOIN_TIMEOUT_SECS);
                        set.shutdown().await;
                        break;
                    }
                }
            }
        }
    }

    Ok(RunResult {
        success_count,
        error_count,
        sum_bytes,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let config = prepare().await?;
    let rlc = RateLimiterConfig::new(config.limiter, config.chunk_kb, config.interval, config.mss);
    let shutdown = install_shutdown_handler();

    let start_time = Instant::now();
    let result = run(&config, rlc, &shutdown).await?;

    info!(
        "All tasks joined. Total elapsed time: {:?}, total GiB: {:.3}, success: {}, error: {}",
        start_time.elapsed(),
        result.sum_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
        result.success_count,
        result.error_count,
    );

    if result.error_count > 0 {
        anyhow::bail!(
            "{} out of {} task(s) failed",
            result.error_count,
            result.success_count + result.error_count
        );
    }

    Ok(())
}
