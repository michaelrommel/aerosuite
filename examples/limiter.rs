use governor::{Quota, RateLimiter};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::TcpStream as StdTcpStream;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> io::Result<()> {
    let listen_addr = "127.0.0.1:8080";
    let target_addr = "127.0.0.1:80";
    const MSS: u32 = 576; // Small MSS to force segmentation
    const KBPS_LIMIT: u32 = 128; // 128 KB/s limit

    let listener = TcpListener::bind(listen_addr).await?;
    // Create a rate limiter shared across all tasks
    let quota = Quota::per_second(NonZeroU32::new(KBPS_LIMIT * 1024).unwrap());
    let limiter = Arc::new(RateLimiter::direct(quota));

    println!(
        "Rate limiting proxy: {} -> {} (MSS: {}, Limit: {}KB/s)",
        listen_addr, target_addr, MSS, KBPS_LIMIT
    );

    loop {
        let (mut client_stream, _) = listener.accept().await?;
        let limiter = Arc::clone(&limiter);

        let target = target_addr;

        tokio::spawn(async move {
            // 1. Manually create outbound socket to set MSS
            let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
            socket.set_tcp_mss(MSS).unwrap();
            socket.set_nonblocking(true).unwrap();

            let std_stream = StdTcpStream::connect(target).unwrap();

            let mut server_stream = match TcpStream::from_std(std_stream) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Connect error: {}", e);
                    return;
                }
            };

            let (mut client_recv, mut client_send) = client_stream.split();
            let (mut server_recv, mut server_send) = server_stream.split();

            // Bridge Server -> Client (with Rate Limiting)
            let downstream = async {
                let mut buf = [0u8; 1024];
                while let Ok(n) = server_recv.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    // The "tc" trick: Wait until the rate limiter allows n bytes
                    limiter
                        .until_n_ready(NonZeroU32::new(n as u32).unwrap())
                        .await
                        .unwrap();
                    client_send.write_all(&buf[..n]).await.unwrap();
                }
            };

            // Bridge Client -> Server (Direct)
            let upstream = io::copy(&mut client_recv, &mut server_send);

            let _ = tokio::join!(downstream, upstream);
        });
    }
}
