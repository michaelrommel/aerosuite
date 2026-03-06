use anyhow::{Context, Result};
use async_stream::stream;
use governor::{Quota, RateLimiter};
use rand::RngExt;
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
    fn new(limiter: bool, size: u32, interval: u64, mss: u32) -> Self {
        Self {
            limiter,
            size,
            interval,
            mss,
        }
    }
}

async fn setup_files() -> Result<u32> {
    let filesize = env::var("AEROSTRESS_SIZE").unwrap_or("10".to_string());
    let s: u32 = filesize
        .parse()
        .expect("AEROSTRESS_SIZE parameter is not a number");
    let file_path = "mediumfile.dat";
    let target_size: u32 = s * 1024 * 1024;
    let mut current_size: u32 = 0;

    let file = File::create(file_path)
        .await
        .context("Temporary file could not be created.")?;
    let mut writer = BufWriter::new(file);
    let mut rng = rand::rng();

    // Using a buffer to speed up writing for large files
    const CHUNK_SIZE: u32 = 8192;
    let mut buffer = [0u8; CHUNK_SIZE as usize];

    while current_size < target_size {
        let remaining: u32 = target_size - current_size;
        let to_write: u32 = std::cmp::min(remaining, CHUNK_SIZE);

        rng.fill(&mut buffer);

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

async fn write_async(
    batch: i32,
    num: i32,
    filename: String,
    destination: String,
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
    println!("Stream {}-{} logged in successfully", batch, num);
    // let mut reader = Cursor::new("Hello from the Rust \"suppaftp\" crate!".as_bytes());
    let mut file = File::open("mediumfile.dat")
        .await
        .with_context(|| format!("Source file {}-{} could not be opened", batch, num))?;
    println!("Stream {}-{} opened source file", batch, num);

    let bytes_written: u64;
    if rlc.limiter {
        ftp_stream = ftp_stream.passive_stream_builder(move |addr: SocketAddr| {
            let fut = async move {
                let socket =
                    Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                if rlc.mss > 0 {
                    socket
                        .set_tcp_mss(rlc.mss)
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
        let mut data_stream = ftp_stream.put_with_stream(filename.clone()).await?;

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
            .put_file(filename.clone(), &mut file)
            .await
            .with_context(|| format!("File {}-{} could not be sent", batch, num))?;
    }
    println!("Stream {}-{} successfully wrote {}", batch, num, filename);
    ftp_stream.quit().await?;
    Ok(bytes_written)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("Creating temporary file to send");
    let file_size = setup_files().await?;
    println!("File created, {} bytes", file_size);

    let mut set: JoinSet<Result<u64>> = JoinSet::new();
    let start_time = Instant::now();
    let target = env::var("AEROSTRESS_TARGET").unwrap_or("127.0.0.1".to_string());
    let batches = env::var("AEROSTRESS_BATCHES").unwrap_or("8".to_string());
    let b: i32 = batches
        .parse()
        .expect("AEROSTRESS_BATCHES parameter is not a number");
    let parallel = env::var("AEROSTRESS_TASKS").unwrap_or("20".to_string());
    let p: i32 = parallel
        .parse()
        .expect("AEROSTRESS_TASKS parameter is not a number");
    let delay = env::var("AEROSTRESS_DELAY").unwrap_or("10".to_string());
    let d: u64 = delay
        .parse()
        .expect("AEROSTRESS_DELAY parameter is not a number");
    let limiter = env::var("AEROSTRESS_LIMITER").unwrap_or("false".to_string());
    let l: bool = limiter
        .parse()
        .expect("AEROSTRESS_LIMITER parameter is not a boolean");
    let chunk = env::var("AEROSTRESS_CHUNK").unwrap_or("4".to_string());
    let c: u32 = chunk
        .parse()
        .expect("AEROSTRESS_CHUNK parameter is not a number");
    let interval = env::var("AEROSTRESS_INTERVAL").unwrap_or("0".to_string());
    let i: u64 = interval
        .parse()
        .expect("AEROSTRESS_INTERVAL parameter is not a number");
    let mss = env::var("AEROSTRESS_MSS").unwrap_or("1460".to_string());
    let m: u32 = mss
        .parse()
        .expect("AEROSTRESS_MSS parameter is not a number");

    let rlc = RateLimiterConfig::new(l, c, i, m);
    let mut error_count: u64 = 0;

    // ramp up the load in steps of batches
    for j in 1..=b {
        println!("Starting {} parallel tasks...", p);
        for i in 1..=p {
            // clone and make destination movable
            let destination = format!("{}:21", target.clone());
            // create an arbitrary start delay for inside the task
            let delay = rand::random_range(1..=75) / 100;
            // start the task immediately
            set.spawn(async move {
                // wait inside the task before starting the ftp transfer
                sleep(Duration::from_secs(delay)).await;
                let f = format!("testfile_{:02}_{:04}.txt", j, i);
                let start_time = Instant::now();
                let bytes_written = write_async(j, i, f, destination, rlc).await?;
                let elapsed = start_time.elapsed();
                println!(
                    "Task {} finished, {:.3} MiBytes, {:.3} kibit/s",
                    i,
                    bytes_written / 1024 / 1024,
                    (bytes_written * 8) as u128 / elapsed.as_millis(),
                );
                Ok(bytes_written)
            });
        }
        println!(
            "Batch {} spawned {:?} seconds after start",
            j,
            start_time.elapsed(),
        );
        // create a delay between batches
        sleep(Duration::from_secs(d)).await;
    }

    let mut sum_bytes = 0;
    // Wait for all spawned tasks to finish
    while let Some(res) = set.join_next().await {
        match res {
            Ok(taskresult) => match taskresult {
                Ok(b) => sum_bytes += b,
                Err(e) => {
                    eprintln!("A write task failed: {:?}", e);
                    error_count += 1;
                }
            },
            Err(e) => eprintln!("A JoinHandle failed: {:?}", e),
        }
    }

    println!(
        "All tasks joined. Total elapsed time: {:?}, total GiB: {:?}, errors: {}",
        start_time.elapsed(),
        sum_bytes / 1024 / 1024 / 1024,
        error_count
    );

    Ok(())
}
