use anyhow::{Context, Result};
use rand::Rng;
use std::{env, time::Instant};
use suppaftp::{tokio::AsyncFtpStream, types::Mode};
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
    task::JoinSet,
    time::{Duration, sleep},
};
use tokio_stream::StreamExt;
use tokio_util::io::{ReaderStream, StreamReader};

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
    brake: u64,
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
    println!("Stream {}-{} read source file", batch, num);

    let bytes_written: u64;
    if brake > 0 {
        let reader_stream = ReaderStream::with_capacity(file, 32 * 1024);
        let throttled_reader = reader_stream.throttle(Duration::from_millis(brake));
        let async_reader = StreamReader::new(throttled_reader);
        tokio::pin!(async_reader);
        let mut data_stream = ftp_stream.put_with_stream(filename.clone()).await?;
        bytes_written = tokio::io::copy(&mut async_reader, &mut data_stream)
            .await
            .with_context(|| format!("File {}-{} could not be streamed", batch, num))?;
        ftp_stream
            .finalize_put_stream(data_stream)
            .await
            .with_context(|| format!("File {}-{} could not be finalized", batch, num))?;
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
    let brake = env::var("AEROSTRESS_THROTTLE").unwrap_or("0".to_string());
    let t: u64 = brake
        .parse()
        .expect("AEROSTRESS_THROTTLE parameter is not a number");

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
                let bytes_written = write_async(j, i, f, destination, t).await?;
                println!(
                    "Task {} finished, {:.3} MBytes",
                    i,
                    bytes_written / 1024 / 1024
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
        "All tasks joined. Total elapsed time: {:?}, total GB: {:?}, errors: {}",
        start_time.elapsed(),
        sum_bytes / 1024 / 1024 / 1024,
        error_count
    );

    Ok(())
}
