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
        use anyhow::{bail, Context};
        use console_subscriber::ConsoleLayer;
        let console_addr: SocketAddr = "127.0.0.1:6669"
            .parse()
            .context("could not parse tokio-console address")?;

        // Convert SocketAddr to the format expected by console_subscriber
        let (ip, port) = match console_addr {
            SocketAddr::V4(addr) => (addr.ip().octets(), addr.port()),
            SocketAddr::V6(_) => bail!("tokio-console only supports IPv4 addresses"),
        };

        ConsoleLayer::builder().server_addr((ip, port)).init();
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
    const BROADCAST_CAPACITY: usize = 32;
    const MPSC_CAPACITY: usize = 32;
    const METRICS_BIND_ADDRESS: &str = "[::]:9090";

    let (shutdown_sender, http_receiver) = tokio::sync::broadcast::channel(BROADCAST_CAPACITY);
    let ftp_shutdown_sender = shutdown_sender.clone();
    let ftp_shutdown_clone = ftp_shutdown_sender.clone();
    let (http_done_sender, mut shutdown_done_received) = tokio::sync::mpsc::channel(MPSC_CAPACITY);
    let ftp_done_sender = http_done_sender.clone();

    // Spawn HTTP metrics server
    tokio::spawn(async move {
        if let Err(e) = http::start(METRICS_BIND_ADDRESS, http_receiver, http_done_sender).await {
            error!("\nHTTP Server error: {}", e);
        }
    });

    // Spawn FTP server
    tokio::spawn(async move {
        if let Err(e) = ftp::start_ftp(ftp_shutdown_clone.subscribe(), ftp_done_sender).await {
            error!("\nFTP Server error: {}", e);
        }
    });

    let signal = signal::listen_for_signals().await?;
    info!("Received signal {}, shutting down...", signal);

    // Drop all senders to trigger graceful shutdown of both servers
    drop(shutdown_sender);
    drop(ftp_shutdown_sender);

    // Wait for HTTP server to complete shutdown (FTP uses same done channel)
    let _ = shutdown_done_received.recv().await;

    Ok(signal)
}
