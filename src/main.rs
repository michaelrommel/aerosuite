/// AeroFTP - FTP server with AWS S3 support and HTTP metrics endpoint.
///
/// This application provides an FTP server that integrates with AWS S3 for file storage,
/// along with a Prometheus-compatible HTTP metrics endpoint on port 9090.
mod aws;
mod ftp;
mod http;
mod metrics;
mod signal;

#[cfg(feature = "tokio_console")]
use std::net::SocketAddr;

use std::process;

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    // let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // let tmp_dir = env::temp_dir();
    // let _tmp_dir = tmp_dir.as_path().to_str().unwrap();

    #[cfg(feature = "tokio_console")]
    {
        use console_subscriber::ConsoleLayer;
        let console_addr: SocketAddr = "127.0.0.1:6669"
            .parse()
            .map_err(|e| format!("could not parse tokio-console address: {}", e))
            .unwrap();

        // Convert SocketAddr to the format expected by console_subscriber
        let (ip, port) = match console_addr {
            SocketAddr::V4(addr) => (addr.ip().octets(), addr.port()),
            SocketAddr::V6(_) => {
                eprintln!("Error: tokio-console only supports IPv4 addresses");
                process::exit(1);
            }
        };

        ConsoleLayer::builder()
            // set the address the server is bound to
            .server_addr((ip, port))
            // ... other configurations ...
            .init();
    }

    if let Err(e) = run().await {
        eprintln!("\nError: {}", e);
        process::exit(1);
    };
}

async fn main_task() -> Result<signal::ExitSignal, String> {
    use anyhow::Error;

    let (shutdown_sender, http_receiver) = tokio::sync::broadcast::channel(1);
    let (http_done_sender, mut shutdown_done_received) = tokio::sync::mpsc::channel(1);
    let ftp_done_sender = http_done_sender.clone();

    let addr = String::from("[::]:9090");
    tokio::spawn(async move {
        if let Err(e) = http::start(&addr, http_receiver, http_done_sender).await {
            println!("HTTP Server error: {}", e)
        }
    });

    ftp::start_ftp(shutdown_sender.subscribe(), ftp_done_sender).await?;

    let signal = signal::listen_for_signals()
        .await
        .map_err(|e: Error| e.to_string())?;
    println!("Received signal {}, shutting down...", signal);

    drop(shutdown_sender);

    // When every sender has gone out of scope, the recv call
    // will return with an error. We ignore the error.
    let _ = shutdown_done_received.recv().await;

    Ok(signal)
}

async fn run() -> Result<(), String> {
    // We wait for a signal (HUP, INT, TERM). If the signal is a HUP,
    // we restart, otherwise we exit the loop and the program ends.
    while main_task().await? == signal::ExitSignal::Hup {
        println!("Restarting on HUP");
    }
    println!("Exiting");
    Ok(())
}
