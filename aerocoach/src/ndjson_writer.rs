//! NDJSON record writer for completed transfer records.
//!
//! Opens one file per test run in `record_dir` with the name format:
//! `<plan_id>_<timestamp>.ndjson`.
//!
//! Each call to [`NdjsonWriter::append`] writes one JSON object (augmented
//! with `agent_id`) followed by a newline.  The writer is created in
//! `POST /start` and flushed/dropped either by the delta ticker when it
//! detects `CoachState::Done`, or in [`crate::state::AppState::reset`].

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::info;

use aeroproto::aeromonitor::TransferRecord;

/// Buffered NDJSON writer for one test run's transfer records.
#[derive(Debug)]
pub struct NdjsonWriter {
    writer: BufWriter<std::fs::File>,
    /// Resolved path to the open file; exposed for `GET /results`.
    pub path: PathBuf,
}

impl NdjsonWriter {
    /// Open a new NDJSON file in `record_dir`.
    ///
    /// Creates `record_dir` and all parent directories if they do not exist.
    /// The filename format is `<plan_id>_<timestamp>.ndjson`.
    pub fn open(record_dir: &Path, prefix: &str) -> Result<Self> {
        std::fs::create_dir_all(record_dir)?;
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        // Sanitise the prefix one final time so the writer is safe even when
        // called from code paths that skipped the main sanitisation step.
        let safe: String = prefix
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let safe = safe.trim_matches('_');
        let safe = if safe.is_empty() { "plan" } else { safe };
        let filename = format!("{safe}_{ts}.ndjson");
        let path = record_dir.join(&filename);
        info!(path = %path.display(), "opening NDJSON record file");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            path,
        })
    }

    /// Append one transfer record as a JSON line.
    ///
    /// The record is enriched with `agent_id` (not present in the proto type).
    pub fn append(&mut self, agent_id: &str, record: &TransferRecord) -> Result<()> {
        let line = serde_json::json!({
            "agent_id":          agent_id,
            "filename":          record.filename,
            "bucket_id":         record.bucket_id,
            "bytes_transferred": record.bytes_transferred,
            "file_size_bytes":   record.file_size_bytes,
            "bandwidth_kibps":   record.bandwidth_kibps,
            "success":           record.success,
            "error_reason":      record.error_reason,
            "start_time_ms":     record.start_time_ms,
            "end_time_ms":       record.end_time_ms,
            "time_slice":        record.time_slice,
        });
        let s = serde_json::to_string(&line)?;
        self.writer.write_all(s.as_bytes())?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }

    /// Flush the write buffer to disk without closing the file.
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush().map_err(Into::into)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(success: bool) -> TransferRecord {
        TransferRecord {
            filename:          "a00_s001_c001_1.dat".into(),
            bucket_id:         "xs".into(),
            bytes_transferred: 1024,
            file_size_bytes:   1024,
            bandwidth_kibps:   512,
            success,
            error_reason:      if success { None } else { Some("550 denied".into()) },
            start_time_ms:     1_000_000,
            end_time_ms:       1_001_000,
            time_slice:        1,
        }
    }

    #[test]
    fn writes_valid_ndjson() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut w = NdjsonWriter::open(dir.path(), "test-plan").expect("open");
        w.append("a00", &make_record(true)).expect("append");
        w.append("a01", &make_record(false)).expect("append");
        w.flush().expect("flush");

        let content = std::fs::read_to_string(&w.path).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(first["agent_id"], "a00");
        assert_eq!(first["success"], true);
        assert_eq!(first["bucket_id"], "xs");

        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("json");
        assert_eq!(second["agent_id"], "a01");
        assert_eq!(second["success"], false);
        assert_eq!(second["error_reason"], "550 denied");
    }

    #[test]
    fn creates_record_dir_if_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a").join("b").join("c");
        assert!(!nested.exists());
        let _w = NdjsonWriter::open(&nested, "plan-x").expect("should create dirs");
        assert!(nested.exists());
    }
}
