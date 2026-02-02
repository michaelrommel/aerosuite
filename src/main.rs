use rand::Rng;
use std::{env, io::Error, time::Instant};
use suppaftp::{tokio::AsyncFtpStream, types::Mode};
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
    task::JoinSet,
    time::{Duration, sleep},
};

async fn setup_files() -> Result<(), Error> {
    let file_path = "mediumfile.dat";
    let target_size: u32 = 100 * 1024 * 1024;
    let mut current_size: u32 = 0;

    let file = File::create(file_path).await?;
    let mut writer = BufWriter::new(file);
    let mut rng = rand::rng();

    // Using a buffer to speed up writing for large files
    const CHUNK_SIZE: u32 = 8192;
    let mut buffer = [0u8; CHUNK_SIZE as usize];

    while current_size < target_size {
        let remaining: u32 = target_size - current_size;
        let to_write: u32 = std::cmp::min(remaining, CHUNK_SIZE);

        rng.fill(&mut buffer);

        writer.write_all(&buffer[..to_write as usize]).await?;
        current_size += to_write;
    }

    writer.flush().await
}

async fn write_async(filename: String, destination: String) -> tokio::io::Result<()> {
    let mut ftp_stream = AsyncFtpStream::connect(destination).await.unwrap();
    ftp_stream.set_mode(Mode::ExtendedPassive);
    ftp_stream.login("test", "secret").await.unwrap();
    // let mut reader = Cursor::new("Hello from the Rust \"suppaftp\" crate!".as_bytes());
    let mut reader = File::open("mediumfile.dat").await?;
    let _ = ftp_stream
        .put_file(filename.clone(), &mut reader)
        .await
        .unwrap();
    println!("Successfully wrote {}", filename);
    ftp_stream.quit().await.unwrap();
    Ok(())
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
async fn main() {
    setup_files().await.unwrap();

    let mut set = JoinSet::new();
    let start_time = Instant::now();
    let target = env::var("AEROSTRESS_TARGET").unwrap_or("127.0.0.1".to_string());
    let parallel = env::var("AEROSTRESS_TASKS").unwrap_or("10".to_string());
    let p: i32 = parallel.parse().expect("TASK parameter is not a number");

    println!("Starting {} parallel tasks...", p);

    for i in 1..=p {
        // clone and make destination movable
        let destination = format!("{}:21", target.clone());
        // create an arbitraty start delay for inside the task
        let delay = rand::random_range(1..=75) / 100;
        // start the task immediately
        set.spawn(async move {
            // wait inside the task before starting the ftp transfer
            sleep(Duration::from_secs(delay)).await;
            let f = format!("testfile_{:04}.txt", i);
            write_async(f, destination).await.unwrap();
            println!("Task {} finished.", i);
        });
    }

    // Wait for all spawned tasks to finish
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            eprintln!("A task failed: {:?}", e);
        }
    }

    println!(
        "All tasks joined. Total elapsed time: {:?}",
        start_time.elapsed()
    );
}
