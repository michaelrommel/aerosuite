//! In-memory accumulation of completed transfer results.
//!
//! The [`MetricsAccumulator`] collects [`TransferOutcome`]s as FTP tasks
//! finish, then drains them into a [`MetricsUpdate`] proto message to be
//! sent to aerocoach via the `Session` stream.

use aeroproto::aeromonitor::{MetricsUpdate, TransferRecord};

use super::transfer::TransferOutcome;

/// Accumulates transfer results between `MetricsUpdate` flushes.
#[derive(Debug, Default)]
pub struct MetricsAccumulator {
    pending: Vec<TransferOutcome>,
}

impl MetricsAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed transfer.
    pub fn record(&mut self, outcome: TransferOutcome) {
        self.pending.push(outcome);
    }

    /// Returns `true` if there are unsent outcomes waiting.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Drain all accumulated outcomes into a [`MetricsUpdate`] ready to send.
    ///
    /// Returns `None` when there are no completed transfers to report, unless
    /// `force` is `true` (used to send a heartbeat even with no completions).
    pub fn drain_into_update(
        &mut self,
        current_slice: u32,
        active_connections: u32,
        force: bool,
    ) -> Option<MetricsUpdate> {
        if self.pending.is_empty() && !force {
            return None;
        }
        let completed_transfers: Vec<TransferRecord> =
            self.pending.drain(..).map(TransferOutcome::into_proto).collect();

        Some(MetricsUpdate {
            current_slice,
            active_connections,
            queued_connections: 0,
            completed_transfers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::transfer::TransferOutcome;

    fn outcome(success: bool) -> TransferOutcome {
        TransferOutcome {
            filename: "f.dat".into(),
            bucket_id: "xs".into(),
            bytes_transferred: 1024,
            file_size_bytes: 1024,
            bandwidth_kibps: 512,
            success,
            error_reason: if success { None } else { Some("err".into()) },
            start_time_ms: 0,
            end_time_ms: 10,
            time_slice: 0,
        }
    }

    #[test]
    fn accumulates_and_drains() {
        let mut acc = MetricsAccumulator::new();
        acc.record(outcome(true));
        acc.record(outcome(false));
        assert!(acc.has_pending());

        let update = acc.drain_into_update(1, 3, false).unwrap();
        assert_eq!(update.completed_transfers.len(), 2);
        assert_eq!(update.current_slice, 1);
        assert_eq!(update.active_connections, 3);
        assert!(!acc.has_pending());
    }

    #[test]
    fn drain_empty_returns_none_without_force() {
        let mut acc = MetricsAccumulator::new();
        assert!(acc.drain_into_update(0, 0, false).is_none());
    }

    #[test]
    fn drain_empty_returns_some_with_force() {
        let mut acc = MetricsAccumulator::new();
        let update = acc.drain_into_update(2, 5, true).unwrap();
        assert_eq!(update.completed_transfers.len(), 0);
        assert_eq!(update.current_slice, 2);
    }

    #[test]
    fn second_drain_is_empty() {
        let mut acc = MetricsAccumulator::new();
        acc.record(outcome(true));
        acc.drain_into_update(0, 1, false);
        assert!(acc.drain_into_update(0, 0, false).is_none());
    }
}
