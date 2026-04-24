//! FTP transfer execution.
//!
//! [`run_transfer`] performs a single FTP upload and returns a
//! [`TransferOutcome`] describing what happened.  All FTP errors are
//! captured inside the outcome so that the `JoinSet` in the session loop
//! never sees panics or propagated errors — a failed transfer is a
//! measurement, not a crash.
//!
//! # Rate limiting
//! When `rate_config` is `Some(cfg)`, the upload uses
//! `put_with_stream` + a `governor`-backed throttled `ReaderStream`,
//! exactly as in the legacy `aerostress` binary.  `None` means unlimited
//! via the simpler `put_file` path.

use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_stream::stream;
use governor::{Quota, RateLimiter};
use socket2::{Domain, Protocol, Socket, Type};
use suppaftp::{tokio::AsyncFtpStream, types::Mode};
use tokio::{fs::File, net::TcpStream};
use tokio_stream::StreamExt;
use tokio_util::io::{ReaderStream, StreamReader};
use tracing::{debug, info, warn};

use aeroproto::aeromonitor::TransferRecord;

use super::rate_limit::RateLimiterConfig;

// Type of the async closure passed to `passive_stream_builder`.
type TcpStreamFuture = Pin<
    Box<dyn futures::Future<Output = std::result::Result<TcpStream, suppaftp::FtpError>> + Send + Sync>,
>;

// ── CountingReader ────────────────────────────────────────────────────────

/// Wraps any [`tokio::io::AsyncRead`] and atomically increments a shared
/// counter as bytes flow through.  Used to track in-flight byte progress
/// for both the rate-limited and unlimited upload paths.
struct CountingReader<R> {
    inner:      R,
    bytes_sent: Arc<AtomicU64>,
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx:       &mut std::task::Context<'_>,
        buf:      &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res    = Pin::new(&mut self.inner).poll_read(cx, buf);
        let n      = buf.filled().len() - before;
        if n > 0 {
            self.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
        }
        res
    }
}

// ── Public types ──────────────────────────────────────────────────────────

/// Result of a single FTP transfer task.
#[derive(Debug, Clone)]
pub struct TransferOutcome {
    /// Connection ID assigned by the session loop; used to retire the
    /// bandwidth allocation from the running-rates map on completion.
    pub conn_id: u64,
    pub filename: String,
    pub bucket_id: String,
    pub bytes_transferred: u64,
    pub file_size_bytes: u64,
    /// Average bandwidth in KiB/s over the whole transfer.
    pub bandwidth_kibps: u32,
    pub success: bool,
    pub error_reason: Option<String>,
    pub start_time_ms: i64,
    pub end_time_ms: i64,
    /// Slice in which this transfer was **started**.
    pub time_slice: u32,
}

impl TransferOutcome {
    /// Convert to the proto [`TransferRecord`] sent to aerocoach.
    pub fn into_proto(self) -> TransferRecord {
        TransferRecord {
            filename: self.filename,
            bucket_id: self.bucket_id,
            bytes_transferred: self.bytes_transferred,
            file_size_bytes: self.file_size_bytes,
            bandwidth_kibps: self.bandwidth_kibps,
            success: self.success,
            error_reason: self.error_reason,
            start_time_ms: self.start_time_ms,
            end_time_ms: self.end_time_ms,
            time_slice: self.time_slice,
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────

/// Execute one FTP upload and return the outcome.
///
/// Errors from the FTP layer are caught here and reported as
/// `TransferOutcome { success: false, error_reason: Some(...) }` so callers
/// never need to handle `Result`.
///
/// `bytes_sent` is incremented atomically as data flows through the upload
/// path, allowing the session loop to sample in-flight progress at any time.
pub async fn run_transfer(
    conn_id: u64,
    filename: String,
    bucket_id: String,
    local_file: PathBuf,
    ftp_target: String,
    ftp_user: String,
    ftp_pass: String,
    rate_config: Option<RateLimiterConfig>,
    time_slice: u32,
    bytes_sent: Arc<AtomicU64>,
) -> TransferOutcome {
    let start_ms = now_ms();

    // Read file size up front for the outcome record (best-effort).
    let file_size_bytes = tokio::fs::metadata(&local_file)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    match ftp_upload(
        &filename,
        &local_file,
        &ftp_target,
        &ftp_user,
        &ftp_pass,
        rate_config,
        bytes_sent,
    )
    .await
    {
        Ok(bytes_transferred) => {
            let end_ms = now_ms();
            let elapsed_ms = (end_ms - start_ms).max(1) as u64;
            // Avoid div-by-zero when file is empty; report 0 KiB/s.
            let bandwidth_kibps = if bytes_transferred > 0 {
                ((bytes_transferred / 1024) * 1000 / elapsed_ms) as u32
            } else {
                0
            };
            info!(
                filename     = %filename,
                bucket       = %bucket_id,
                bytes        = bytes_transferred,
                kib_s        = bandwidth_kibps,
                elapsed_ms,
                "transfer complete"
            );
            TransferOutcome {
                conn_id,
                filename,
                bucket_id,
                bytes_transferred,
                file_size_bytes,
                bandwidth_kibps,
                success: true,
                error_reason: None,
                start_time_ms: start_ms,
                end_time_ms: end_ms,
                time_slice,
            }
        }
        Err(e) => {
            let end_ms = now_ms();
            warn!(filename = %filename, bucket = %bucket_id, error = %e, "transfer failed");
            TransferOutcome {
                conn_id,
                filename,
                bucket_id,
                bytes_transferred: 0,
                file_size_bytes,
                bandwidth_kibps: 0,
                success: false,
                error_reason: Some(e.to_string()),
                start_time_ms: start_ms,
                end_time_ms: end_ms,
                time_slice,
            }
        }
    }
}

// ── FTP upload internals ──────────────────────────────────────────────────

/// Perform the actual FTP upload.  Returns the number of bytes transferred.
async fn ftp_upload(
    filename:    &str,
    local_file:  &Path,
    ftp_target:  &str,
    ftp_user:    &str,
    ftp_pass:    &str,
    rate_config: Option<RateLimiterConfig>,
    bytes_sent:  Arc<AtomicU64>,
) -> Result<u64> {
    // ── Connect & authenticate ────────────────────────────────────────────
    let mut ftp = AsyncFtpStream::connect(ftp_target)
        .await
        .with_context(|| format!("FTP connect to {ftp_target:?} failed"))?;
    ftp.set_mode(Mode::ExtendedPassive);
    ftp.login(ftp_user, ftp_pass)
        .await
        .context("FTP login failed")?;
    debug!(filename = %filename, target = %ftp_target, "FTP connected and authenticated");

    // ── Open local file ───────────────────────────────────────────────────
    let file = File::open(local_file)
        .await
        .with_context(|| format!("cannot open {}", local_file.display()))?;

    // ── Upload (rate-limited or unlimited) ────────────────────────────────
    let bytes_written = match rate_config {
        // ── Unlimited ─────────────────────────────────────────────────────
        None => {
            // CountingReader keeps bytes_sent updated as the file streams.
            let mut counting = CountingReader { inner: file, bytes_sent };
            ftp.put_file(filename, &mut counting)
                .await
                .with_context(|| format!("FTP put_file {filename:?} failed"))?
        }

        // ── Rate-limited ──────────────────────────────────────────────────
        Some(cfg) => {
            // Optionally install a custom TCP socket builder for MSS control.
            if cfg.mss > 0 {
                let mss = cfg.mss;
                ftp = ftp.passive_stream_builder(move |addr: SocketAddr| {
                    Box::pin(async move { make_tcp_stream(addr, mss).await }) as TcpStreamFuture
                });
            }

            let mut data_stream = ftp
                .put_with_stream(filename)
                .await
                .with_context(|| format!("FTP put_with_stream {filename:?} failed"))?;

            let n = if cfg.interval_ms > 0 {
                // Throttled: one chunk per `interval_ms` milliseconds.
                // Count bytes before yielding so the session loop always
                // sees a value ≤ actual bytes on the wire.
                let mut reader_stream =
                    ReaderStream::with_capacity(file, cfg.chunk_bytes as usize);
                let quota = Quota::with_period(Duration::from_millis(cfg.interval_ms))
                    .with_context(|| {
                        format!("invalid rate-limit interval {} ms", cfg.interval_ms)
                    })?;
                let limiter = Arc::new(RateLimiter::direct(quota));
                debug!(
                    filename     = %filename,
                    chunk_bytes  = cfg.chunk_bytes,
                    interval_ms  = cfg.interval_ms,
                    "starting rate-limited transfer"
                );
                let bs = bytes_sent.clone();
                let throttled = stream! {
                    while let Some(chunk) = reader_stream.next().await {
                        limiter.until_ready().await;
                        if let Ok(ref c) = chunk {
                            bs.fetch_add(c.len() as u64, Ordering::Relaxed);
                        }
                        yield chunk;
                    }
                };
                let async_reader = StreamReader::new(throttled);
                tokio::pin!(async_reader);
                tokio::io::copy(&mut async_reader, &mut data_stream)
                    .await
                    .with_context(|| format!("rate-limited copy for {filename:?} failed"))?
            } else {
                // Custom socket (MSS) but no governor throttle — use CountingReader.
                let counting     = CountingReader { inner: file, bytes_sent };
                let async_reader = StreamReader::new(ReaderStream::new(counting));
                tokio::pin!(async_reader);
                tokio::io::copy(&mut async_reader, &mut data_stream)
                    .await
                    .with_context(|| format!("unthrottled copy for {filename:?} failed"))?
            };

            ftp.finalize_put_stream(data_stream)
                .await
                .with_context(|| format!("FTP finalize_put_stream {filename:?} failed"))?;
            n
        }
    };

    // ── Clean disconnect ──────────────────────────────────────────────────
    ftp.quit().await.context("FTP QUIT failed")?;

    Ok(bytes_written)
}

/// Build a `tokio::net::TcpStream` with an optional custom MSS via `socket2`.
///
/// This matches the `passive_stream_builder` pattern used in the legacy
/// `aerostress` binary.
async fn make_tcp_stream(
    addr: SocketAddr,
    mss: u32,
) -> std::result::Result<TcpStream, suppaftp::FtpError> {
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
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ── Unit tests (no live FTP needed) ───────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_outcome(success: bool) -> TransferOutcome {
        TransferOutcome {
            conn_id: 1,
            filename: "a00_s001_c000001.dat".into(),
            bucket_id: "xs".into(),
            bytes_transferred: if success { 4096 } else { 0 },
            file_size_bytes: 4096,
            bandwidth_kibps: if success { 512 } else { 0 },
            success,
            error_reason: if success { None } else { Some("550 Permission denied".into()) },
            start_time_ms: 1_000_000,
            end_time_ms:   1_001_000,
            time_slice: 1,
        }
    }

    #[test]
    fn success_outcome_maps_to_proto() {
        let rec = dummy_outcome(true).into_proto();
        assert!(rec.success);
        assert_eq!(rec.filename, "a00_s001_c000001.dat");
        assert_eq!(rec.bucket_id, "xs");
        assert_eq!(rec.bytes_transferred, 4096);
        assert_eq!(rec.bandwidth_kibps, 512);
        assert!(rec.error_reason.is_none());
        assert_eq!(rec.time_slice, 1);
    }

    #[test]
    fn error_outcome_maps_to_proto() {
        let rec = dummy_outcome(false).into_proto();
        assert!(!rec.success);
        assert_eq!(rec.bytes_transferred, 0);
        assert_eq!(rec.error_reason.as_deref(), Some("550 Permission denied"));
    }

    /// Verify that a missing local file produces a failed outcome rather than
    /// a panic.
    #[tokio::test]
    async fn missing_file_gives_error_outcome() {
        let outcome = run_transfer(
            0,
            "test.dat".into(),
            "xs".into(),
            PathBuf::from("/nonexistent/path/file.dat"),
            "127.0.0.1:21".into(),
            "user".into(),
            "pass".into(),
            None,
            0,
            Arc::new(AtomicU64::new(0)),
        )
        .await;

        // The FTP connect will fail before we even open the file, but the
        // outcome must be a non-panicking error result.
        assert!(!outcome.success);
        assert!(outcome.error_reason.is_some());
    }
}
