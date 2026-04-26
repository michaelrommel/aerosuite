//! Shared runtime state protected by a [`tokio::sync::RwLock`].
//!
//! [`AppState`] is the single source of truth for the coach's current
//! operational status, connected agents, active load plan, and accumulated
//! metrics.  It is wrapped in an [`Arc`] and shared across the gRPC server,
//! HTTP/WebSocket server, and slice clock tasks.
//!
//! ```text
//! Arc<RwLock<AppState>>
//!        │
//!        ├── CoachState  (WAITING / RUNNING / DONE)
//!        ├── Option<LoadPlanFile>
//!        ├── Registry    (connected agents + their gRPC send channels)
//!        ├── MetricsStore (accumulated TransferRecords)
//!        └── …
//! ```

pub mod delta;
pub mod metrics_store;
pub mod registry;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, watch, Notify, RwLock};

use crate::model::LoadPlanFile;
use crate::ndjson_writer::NdjsonWriter;

/// Capacity of the WebSocket broadcast channel (JSON strings).
const WS_BROADCAST_CAPACITY: usize = 32;

// ── Coach state machine ────────────────────────────────────────────────────

/// Top-level state of the aerocoach process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoachState {
    /// Accepting agent registrations; plan can still be replaced via PUT /plan.
    /// Test will not start until the operator sends POST /start.
    Waiting,

    /// Test is running; slice clock is active.
    Running {
        /// Index of the slice currently being executed (0-based).
        current_slice: u32,
    },

    /// All slices completed (or POST /stop was received).
    /// Result file has been written; GET /results is now available.
    Done,
}

impl CoachState {
    pub fn is_waiting(&self) -> bool {
        matches!(self, Self::Waiting)
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }

    /// Return the current slice index, if running.
    pub fn current_slice(&self) -> Option<u32> {
        match self {
            Self::Running { current_slice } => Some(*current_slice),
            _ => None,
        }
    }
}

impl std::fmt::Display for CoachState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Waiting => write!(f, "WAITING"),
            Self::Running { current_slice } => write!(f, "RUNNING(slice={current_slice})"),
            Self::Done => write!(f, "DONE"),
        }
    }
}

// ── Shared application state ───────────────────────────────────────────────

/// All mutable runtime state for one aerocoach process.
///
/// Obtain a reference via [`SharedState`] (`Arc<RwLock<AppState>>`).
#[derive(Debug)]
pub struct AppState {
    /// Current phase of the coach state machine.
    pub coach_state: CoachState,

    /// Active load plan (populated at startup from file, or via PUT /plan).
    pub load_plan: Option<LoadPlanFile>,

    pub registry: registry::Registry,
    pub metrics:  metrics_store::MetricsStore,

    /// Notified (via [`AppState::signal_ack`]) whenever any agent sends a
    /// `SliceAck`.  The slice clock waits on this to avoid spinning.
    pub ack_notify: Arc<Notify>,

    /// Stop signal.  Send `true` to request early termination of the clock.
    /// Use [`AppState::signal_stop`] — no write lock required.
    stop_tx: watch::Sender<bool>,

    // ── WebSocket broadcast ───────────────────────────────────────────────

    /// Sender half of the WebSocket fan-out channel.  The delta ticker sends
    /// serialised `DashboardUpdate` JSON here every ~3 s.  Each connected
    /// aerotrack client holds a matching `Receiver`.
    pub ws_tx: broadcast::Sender<String>,

    // ── NDJSON record writer ──────────────────────────────────────────────

    /// Directory where NDJSON result files are written (from env config).
    pub record_dir: PathBuf,

    /// Optional directory of JSON plan files, set from `AEROCOACH_PLAN_DIR`.
    /// `None` when the single-file mode (`AEROCOACH_PLAN_FILE`) is used.
    pub plan_dir: Option<PathBuf>,

    /// Filesystem stem of the plan file that is currently loaded, e.g.
    /// `"01_parallel_fast"` for `01_parallel_fast.json`.  Used as the prefix
    /// for NDJSON record file names so the filename on disk is predictable and
    /// never contains characters that are illegal on any common filesystem.
    /// `None` when the plan was supplied via `PUT /plan` (no backing file).
    pub plan_filename_stem: Option<String>,

    /// Open record writer for the current test run.  `Some` from
    /// `POST /start` until the delta ticker flushes it on DONE, or until
    /// `reset()` drops it.
    pub record_writer: Option<NdjsonWriter>,

    /// Path of the NDJSON file for the most recent (or current) test run.
    /// Used by `GET /results` to serve the download.
    pub record_file_path: Option<PathBuf>,
}

impl AppState {
    pub fn new() -> Self {
        let (stop_tx, _)      = watch::channel(false);
        let (ws_tx, _ws_rx_0) = broadcast::channel(WS_BROADCAST_CAPACITY);
        Self {
            coach_state: CoachState::Waiting,
            load_plan:   None,
            registry:         registry::Registry::new(),
            metrics:          metrics_store::MetricsStore::new(),
            ack_notify:       Arc::new(Notify::new()),
            stop_tx,
            ws_tx,
            record_dir:         PathBuf::from("/data/records"),
            plan_dir:           None,
            plan_filename_stem: None,
            record_writer:      None,
            record_file_path:   None,
        }
    }

    /// Reset to WAITING, clearing all agent registrations and accumulated
    /// metrics.  The load plan is preserved so the same test can be re-run
    /// immediately.  Call only when `coach_state` is `Done`.
    pub fn reset(&mut self) {
        // Flush any remaining buffered records before dropping the writer.
        if let Some(ref mut w) = self.record_writer {
            let _ = w.flush();
        }
        self.record_writer    = None;
        self.record_file_path = None;

        self.coach_state = CoachState::Waiting;
        self.registry    = registry::Registry::new();
        self.metrics     = metrics_store::MetricsStore::new();
        self.ack_notify  = Arc::new(Notify::new());
        self.reset_stop();
    }

    /// How many agents are currently registered (connected or not).
    #[allow(dead_code)]
    pub fn agent_count(&self) -> usize {
        self.registry.len()
    }

    // ── Signal helpers (callable under a *read* lock) ──────────────────────

    /// Wake the slice clock's ack-wait loop.  Call after recording a
    /// `SliceAck` in the registry.
    ///
    /// Takes `&self` — no write lock required.
    pub fn signal_ack(&self) {
        self.ack_notify.notify_waiters();
    }

    /// Request early termination of the running slice clock.
    ///
    /// Takes `&self` — no write lock required.
    pub fn signal_stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    /// Reset the stop signal (called when a new test starts).
    pub fn reset_stop(&self) {
        let _ = self.stop_tx.send(false);
    }

    /// Replace the stop channel with a brand-new one and return the receiver.
    ///
    /// Call this inside the write lock when transitioning to `Running` so the
    /// clock task always gets a receiver from a channel that has never had
    /// `true` sent on it.  The old sender is dropped; any receivers from the
    /// previous test run would see `RecvError`, but by the time this is called
    /// the previous clock has already finished and no live receivers exist.
    pub fn renew_stop(&mut self) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        self.stop_tx = tx;
        rx
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience alias: the shared, lock-protected application state.
pub type SharedState = Arc<RwLock<AppState>>;

/// Construct a new [`SharedState`] ready for use.
pub fn new_shared_state() -> SharedState {
    Arc::new(RwLock::new(AppState::new()))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_predicates() {
        assert!(CoachState::Waiting.is_waiting());
        assert!(!CoachState::Waiting.is_running());
        assert!(!CoachState::Waiting.is_done());

        let running = CoachState::Running { current_slice: 3 };
        assert!(running.is_running());
        assert_eq!(running.current_slice(), Some(3));

        assert!(CoachState::Done.is_done());
        assert_eq!(CoachState::Done.current_slice(), None);
    }

    #[test]
    fn display() {
        assert_eq!(CoachState::Waiting.to_string(), "WAITING");
        assert_eq!(
            CoachState::Running { current_slice: 2 }.to_string(),
            "RUNNING(slice=2)"
        );
        assert_eq!(CoachState::Done.to_string(), "DONE");
    }

    #[test]
    fn new_state_is_waiting_with_no_plan() {
        let state = AppState::new();
        assert!(state.coach_state.is_waiting());
        assert!(state.load_plan.is_none());
        assert_eq!(state.agent_count(), 0);
    }

    #[test]
    fn stop_signal_round_trip() {
        let mut state = AppState::new();
        // renew_stop() gives a fresh receiver and is how start_handler obtains one.
        let mut rx = state.renew_stop();
        assert!(!*rx.borrow()); // fresh channel starts at false
        state.signal_stop();
        assert!(*rx.borrow_and_update()); // now true
        state.reset_stop();
        assert!(!*rx.borrow_and_update()); // reset
    }
}
