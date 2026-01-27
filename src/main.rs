use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;
// use suppaftp::FtpStream;
use suppaftp::Mode;
use suppaftp::tokio::AsyncFtpStream;
use tokio::task::JoinSet;
use tokio::time::{Duration, sleep};

async fn write_async(filename: String, buffer: &Vec<u8>) -> tokio::io::Result<()> {
    // let mut ftp_stream = AsyncFtpStream::connect("172.17.2.23:2121").await.unwrap();
    let mut ftp_stream = AsyncFtpStream::connect("[fc00:1234:0:0:13:0:0:13]:2121")
        .await
        .unwrap();
    ftp_stream.login("rdiagftp", "siemens").await.unwrap();
    ftp_stream.set_mode(Mode::ExtendedPassive);
    let mut reader = Cursor::new(buffer);
    // let mut reader = File::open("mediumfile.dat").await?;
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
    // write_sync();

    // read a file into memory
    let buffer = Arc::new(tokio::fs::read("mediumfile.dat").await.unwrap());

    let mut set = JoinSet::new();
    let start_time = Instant::now();

    println!("Starting xxx parallel tasks...");

    for i in 0..100 {
        let content_ref = Arc::clone(&buffer);
        set.spawn(async move {
            let f = format!("greeting_{:04}.txt", i);
            let _ = write_async(f, &content_ref).await;
            // let delay = rand::random_range(1..5) / 10;
            // sleep(Duration::from_secs(delay)).await;
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
