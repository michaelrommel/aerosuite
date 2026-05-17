//! OpenTelemetry tracing bridge for FTP data events.
//!
//! [`FtpDataTracer`] implements libunftp's [`DataListener`] trait.  Every time
//! libunftp completes a STOR, RETR, DELE, RNTO, MKD or RMD command it calls
//! [`receive_data_event`](DataListener::receive_data_event), which we intercept
//! to emit a short-lived `tracing` span.
//!
//! Because `tracing_opentelemetry` is installed as a global subscriber layer,
//! every span created here is forwarded to the OTLP exporter and ends up in
//! Grafana Tempo as a searchable OTEL span.  The `ftp.path` field on each span
//! is the primary search key:
//!
//! ```text
//! { span.ftp.path =~ ".*flight_data.*" }           // TraceQL in Explore → Tempo
//! { span.ftp.username = "uploader" && span.ftp.bytes > 1000000 }
//! ```
//!
//! `ftp.session_id` carries libunftp's internal `trace_id` UUID, which matches
//! the value logged by libunftp's own slog output — use it to correlate CloudWatch
//! log lines with the corresponding Tempo spans.

use async_trait::async_trait;
use libunftp::notification::{DataEvent, DataListener, EventMeta};
use tracing::info_span;

/// Emits one OTEL span per completed FTP data event.
///
/// Each span has a near-zero duration (it is opened and immediately closed
/// inside the notification callback) but carries structured attributes that
/// Tempo indexes and makes searchable via TraceQL.
///
/// Registered with the server builder via
/// [`ServerBuilder::notify_data`](libunftp::ServerBuilder::notify_data).
#[derive(Debug)]
pub struct FtpDataTracer;

#[async_trait]
impl DataListener for FtpDataTracer {
    async fn receive_data_event(&self, event: DataEvent, meta: EventMeta) {
        match event {
            // ── STOR ──────────────────────────────────────────────────────────
            DataEvent::Put { path, bytes } => {
                let _span = info_span!(
                    "ftp.stor",
                    ftp.path       = %path,
                    ftp.bytes      = bytes,
                    ftp.username   = %meta.username,
                    ftp.session_id = %meta.trace_id,
                    ftp.sequence   = meta.sequence_number,
                )
                .entered();
            }

            // ── RETR ──────────────────────────────────────────────────────────
            DataEvent::Got { path, bytes } => {
                let _span = info_span!(
                    "ftp.retr",
                    ftp.path       = %path,
                    ftp.bytes      = bytes,
                    ftp.username   = %meta.username,
                    ftp.session_id = %meta.trace_id,
                    ftp.sequence   = meta.sequence_number,
                )
                .entered();
            }

            // ── DELE ──────────────────────────────────────────────────────────
            DataEvent::Deleted { path } => {
                let _span = info_span!(
                    "ftp.dele",
                    ftp.path       = %path,
                    ftp.username   = %meta.username,
                    ftp.session_id = %meta.trace_id,
                    ftp.sequence   = meta.sequence_number,
                )
                .entered();
            }

            // ── RNFR / RNTO ───────────────────────────────────────────────────
            DataEvent::Renamed { from, to } => {
                let _span = info_span!(
                    "ftp.rename",
                    ftp.path       = %to,
                    ftp.path_from  = %from,
                    ftp.username   = %meta.username,
                    ftp.session_id = %meta.trace_id,
                    ftp.sequence   = meta.sequence_number,
                )
                .entered();
            }

            // ── MKD / RMD ─────────────────────────────────────────────────────
            DataEvent::MadeDir { path } => {
                let _span = info_span!(
                    "ftp.mkd",
                    ftp.path       = %path,
                    ftp.username   = %meta.username,
                    ftp.session_id = %meta.trace_id,
                )
                .entered();
            }

            DataEvent::RemovedDir { path } => {
                let _span = info_span!(
                    "ftp.rmd",
                    ftp.path       = %path,
                    ftp.username   = %meta.username,
                    ftp.session_id = %meta.trace_id,
                )
                .entered();
            }
        }
    }
}
