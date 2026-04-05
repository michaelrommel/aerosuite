//! AeroFTP - A secure FTP server with AWS credential support and HTTP metrics.
//!
//! This program implements an FTP server that:
//! - Serves files over FTP on port 21
//! - Exposes Prometheus-compatible metrics on HTTP (default: [::]:9090)
//! - Supports graceful shutdown via HUP, INT, and TERM signals
//! - Automatically restarts on HUP signal, exits on INT/TERM
//! - Uses cached AWS credentials from EC2 metadata, ECS, or EKS providers

mod aws;
mod ftp;
mod http;
mod metrics;
mod signal;

use log::{error, info};

#[cfg(feature = "tokio_console")]
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    #[cfg(feature = "tokio_console")]
    {
        use anyhow::{Context, bail};
        use console_subscriber::ConsoleLayer;
        let console_addr: SocketAddr = "127.0.0.1:6669"
            .parse()
            .context("could not parse tokio-console address")?;

        // Convert SocketAddr to the format expected by console_subscriber
        let (ip, port) = match console_addr {
            SocketAddr::V4(addr) => (addr.ip().octets(), addr.port()),
            SocketAddr::V6(_) => bail!("tokio-console only supports IPv4 addresses"),
        };

        ConsoleLayer::builder()
            // set the address the server is bound to
            .server_addr((ip, port))
            // ... other configurations ...
            .init();
    }

    run().await?;

    Ok(())
}

/// Execute the main application loop.
///
/// Spawns the HTTP metrics server, starts the FTP server, and listens for
/// signals. Restarts on HUP signal, exits on INT/TERM.
///
/// # Returns
/// * `Ok(())` - Application exited normally
async fn run() -> anyhow::Result<()> {
    // We wait for a signal (HUP, INT, TERM). If the signal is a HUP,
    // we restart, otherwise we exit the loop and the program ends.
    while main_task().await? == signal::ExitSignal::Hup {
        info!("Restarting on HUP");
    }
    info!("Exiting");
    Ok(())
}

/// Execute one iteration of the main task.
///
/// Starts HTTP metrics server, FTP server, and waits for signals.
///
/// # Returns
/// * `Ok(ExitSignal::Hup)` - Restart requested
/// * `Ok(ExitSignal::Int|Term)` - Exit requested
async fn main_task() -> anyhow::Result<signal::ExitSignal> {
    let (shutdown_sender, http_receiver) = tokio::sync::broadcast::channel(1);
    let (http_done_sender, mut shutdown_done_received) = tokio::sync::mpsc::channel(1);
    let ftp_done_sender = http_done_sender.clone();

    let addr = String::from("[::]:9090");
    tokio::spawn(async move {
        if let Err(e) = http::start(&addr, http_receiver, http_done_sender).await {
            error!("HTTP Server error: {}", e);
        }
    });

    ftp::start_ftp(shutdown_sender.subscribe(), ftp_done_sender).await?;

    let signal = signal::listen_for_signals().await?;
    info!("Received signal {}, shutting down...", signal);

    drop(shutdown_sender);

    // When every sender has gone out of scope, the recv call
    // will return with an error. We ignore the error.
    let _ = shutdown_done_received.recv().await;

    Ok(signal)
}
