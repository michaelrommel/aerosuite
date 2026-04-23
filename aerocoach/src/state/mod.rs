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

use std::sync::Arc;

use tokio::sync::{watch, Notify, RwLock};

use crate::model::LoadPlanFile;

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
}

impl AppState {
    pub fn new() -> Self {
        let (stop_tx, _) = watch::channel(false);
        Self {
            coach_state: CoachState::Waiting,
            load_plan:   None,
            registry:    registry::Registry::new(),
            metrics:     metrics_store::MetricsStore::new(),
            ack_notify:  Arc::new(Notify::new()),
            stop_tx,
        }
    }

    /// How many agents are currently registered (connected or not).
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

    /// Create a new [`watch::Receiver`] for the stop signal.
    ///
    /// Pass to [`crate::model::clock::SliceClock::new`] before spawning the
    /// clock task.
    pub fn subscribe_stop(&self) -> watch::Receiver<bool> {
        self.stop_tx.subscribe()
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
        let state = AppState::new();
        let mut rx = state.subscribe_stop();
        assert!(!*rx.borrow()); // initially false
        state.signal_stop();
        assert!(*rx.borrow_and_update()); // now true
        state.reset_stop();
        assert!(!*rx.borrow_and_update()); // reset
    }
}
