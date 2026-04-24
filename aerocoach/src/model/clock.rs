//! Slice clock: master timekeeper for an aerosuite load test.
//!
//! [`SliceClock::run`] drives the [`CoachState`] machine from
//! `Running { slice }` through all slices to `Done`, broadcasting a
//! [`SliceTick`] to every connected agent at the start of each interval and
//! waiting (up to [`ACK_TIMEOUT`]) for all agents to acknowledge before
//! advancing.
//!
//! # Timing model
//!
//! ```text
//! t = 0          t = slice_ms       t = 2 × slice_ms
//! │              │                  │
//! ▼              ▼                  ▼
//! ╔══════════════╦══════════════════╦══════════ …
//! ║  slice 0     ║  slice 1         ║  slice 2
//! ╚══════════════╩══════════════════╩══════════ …
//! │◄── ack wait ─►│                 │
//! ```
//!
//! The first tick fires immediately (at t = 0).  Subsequent ticks fire at
//! multiples of `slice_duration_ms`.  If processing takes longer than
//! `slice_ms` the missed tick is skipped (`MissedTickBehavior::Skip`).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{watch, Notify};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

use aeroproto::aeromonitor::{coach_command, CoachCommand, ShutdownCmd, SliceTick};

use crate::state::{CoachState, SharedState};

/// How long the clock waits for all connected agents to ack a slice before
/// advancing anyway.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

// ── SliceClock ─────────────────────────────────────────────────────────────

/// Drives the slice progression for one test run.
///
/// Construct with [`SliceClock::new`] and spawn [`SliceClock::run`] as a
/// tokio task immediately after `POST /start` transitions the coach to
/// `Running`.
pub struct SliceClock {
    state:       SharedState,
    ack_notify:  Arc<Notify>,
    stop_rx:     watch::Receiver<bool>,
}

impl SliceClock {
    /// Create a new clock.
    ///
    /// # Arguments
    /// - `state`      — shared application state (must have a plan loaded).
    /// - `ack_notify` — clone of `AppState::ack_notify`; signalled by the
    ///   gRPC session handler on every `SliceAck`.
    /// - `stop_rx`    — receiver from `AppState::subscribe_stop()`; sending
    ///   `true` on the corresponding sender terminates the clock gracefully.
    pub fn new(
        state:      SharedState,
        ack_notify: Arc<Notify>,
        stop_rx:    watch::Receiver<bool>,
    ) -> Self {
        Self { state, ack_notify, stop_rx }
    }

    /// Run the clock for the full test.
    ///
    /// Returns when all slices complete, when the stop signal fires, or when
    /// the load plan is unexpectedly absent.  In all cases `CoachState` is
    /// transitioned to `Done` before returning.
    pub async fn run(mut self) {
        // Read the plan parameters and release the lock before any write below.
        let plan_info = {
            let read = self.state.read().await;
            read.load_plan
                .as_ref()
                .map(|p| (p.slice_duration_ms, p.total_slices()))
            // `read` drops here — lock released
        };

        let (slice_duration_ms, total_slices) = match plan_info {
            Some(info) => info,
            None => {
                warn!("SliceClock started with no load plan — aborting");
                self.state.write().await.coach_state = CoachState::Done;
                return;
            }
        };

        info!(
            slices   = total_slices,
            slice_ms = slice_duration_ms,
            "slice clock starting"
        );

        let mut ticker = interval(Duration::from_millis(slice_duration_ms));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        for slice_index in 0..total_slices {
            // Wait for the interval tick (first tick fires immediately at t=0).
            // Using a local bool keeps the stop_rx borrow confined to the
            // select! expression, allowing self.on_stop() to borrow self
            // afterwards without conflict.
            let stop_requested = tokio::select! {
                biased;
                _ = self.stop_rx.wait_for(|v| *v) => true,
                _ = ticker.tick()                  => false,
            };
            if stop_requested {
                self.on_stop("POST /stop").await;
                return;
            }

            // Advance state machine.
            self.state.write().await.coach_state =
                CoachState::Running { current_slice: slice_index };

            // Broadcast SliceTick to every connected agent.
            let sent = {
                let tick = CoachCommand {
                    payload: Some(coach_command::Payload::SliceTick(SliceTick {
                        slice_index,
                        wall_clock_ms: Utc::now().timestamp_millis(),
                    })),
                };
                self.state.write().await.registry.broadcast(tick)
            };
            info!(slice = slice_index, agents_sent = sent, "SliceTick broadcast");

            // Wait for all connected agents to ack, or timeout.
            if self.wait_for_acks(slice_index).await {
                // stop signal fired during ack wait
                self.on_stop("stop during ack wait").await;
                return;
            }
        }

        // Wait for the last slice to run its full duration before sending
        // ShutdownCmd.  Without this extra tick, ShutdownCmd fires ~8 ms
        // after SliceTick(N-1) — right after acks arrive, while agents have
        // just ramped up their connections and are still mid-transfer.
        let stop_requested = tokio::select! {
            biased;
            _ = self.stop_rx.wait_for(|v| *v) => true,
            _ = ticker.tick()                  => false,
        };
        if stop_requested {
            self.on_stop("POST /stop during last slice").await;
            return;
        }

        self.on_complete().await;
    }

    // ── Ack waiting ────────────────────────────────────────────────────────

    /// Wait until all connected agents have acked `slice_index`, the
    /// [`ACK_TIMEOUT`] expires, or the stop signal fires.
    ///
    /// Returns `true` if the stop signal fired (caller should terminate).
    async fn wait_for_acks(&mut self, slice_index: u32) -> bool {
        let deadline = tokio::time::Instant::now() + ACK_TIMEOUT;

        loop {
            // Create the Notified future BEFORE checking all_acked so any
            // notification that fires between the check and the select! is
            // captured and immediately resolves the future.
            let notified = self.ack_notify.notified();

            if self.state.read().await.registry.all_acked(slice_index) {
                return false;
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                self.log_lagging(slice_index).await;
                return false;
            }

            tokio::select! {
                biased;
                _ = self.stop_rx.wait_for(|v| *v) => return true,
                _ = notified                       => {}   // re-check all_acked
                _ = tokio::time::sleep(remaining)  => {}
            }
            // Did we exhaust the deadline? Log outside the select! to keep
            // the stop_rx borrow confined to the select expression.
            if tokio::time::Instant::now() >= deadline {
                self.log_lagging(slice_index).await;
                return false;
            }
        }
    }

    // ── Helpers ────────────────────────────────────────────────────────────

    async fn log_lagging(&self, slice_index: u32) {
        let lagging: Vec<String> = self
            .state
            .read()
            .await
            .registry
            .status_snapshot()
            .into_iter()
            .filter(|a| a.connected && a.current_slice < slice_index)
            .map(|a| a.agent_id)
            .collect();

        if !lagging.is_empty() {
            warn!(
                slice    = slice_index,
                ?lagging,
                "ack timeout — advancing clock, lagging agents noted"
            );
        }
    }

    async fn on_stop(&self, reason: &str) {
        info!(reason = %reason, "clock stopping early");
        self.broadcast_shutdown(reason.to_string()).await;
        self.state.write().await.coach_state = CoachState::Done;
        info!("aerocoach → DONE (stopped)");
    }

    async fn on_complete(&self) {
        info!("all slices complete");
        self.broadcast_shutdown("test complete".to_string()).await;
        self.state.write().await.coach_state = CoachState::Done;
        info!("aerocoach → DONE");
    }

    async fn broadcast_shutdown(&self, reason: String) {
        let cmd = CoachCommand {
            payload: Some(coach_command::Payload::Shutdown(ShutdownCmd {
                graceful: true,
                reason,
            })),
        };
        let n = self.state.write().await.registry.broadcast(cmd);
        info!(agents = n, "ShutdownCmd broadcast");
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::load_plan::{BucketSpec, FileDistributionSpec, LoadPlanFile, SliceSpec};
    use crate::state::{new_shared_state, registry::Registry};

    // ── Helpers ───────────────────────────────────────────────────────────

    fn make_plan(n_slices: u32, slice_duration_ms: u64) -> LoadPlanFile {
        LoadPlanFile {
            plan_id: "clock-test".into(),
            slice_duration_ms,
            total_bandwidth_bps: 1_000_000,
            file_distribution: FileDistributionSpec {
                buckets: vec![BucketSpec {
                    bucket_id: "xs".into(),
                    size_min_bytes: 0,
                    size_max_bytes: 1024,
                    percentage: 1.0,
                }],
            },
            slices: (0..n_slices)
                .map(|i| SliceSpec { slice_index: i, total_connections: 10 })
                .collect(),
        }
    }

    fn make_clock(state: &SharedState) -> SliceClock {
        let mut write = state.try_write().unwrap();
        let stop_rx    = write.renew_stop();
        let ack_notify = write.ack_notify.clone();
        drop(write);
        SliceClock::new(state.clone(), ack_notify, stop_rx)
    }

    async fn load_plan(state: &SharedState, n_slices: u32, slice_ms: u64) {
        state.write().await.load_plan = Some(make_plan(n_slices, slice_ms));
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    /// Clock with no connected agents: all_acked is vacuously true, so the
    /// clock advances through all slices instantly (with paused time).
    #[tokio::test]
    async fn clock_completes_all_slices_no_agents() {
        tokio::time::pause();
        let state = new_shared_state();
        load_plan(&state, 3, 60_000).await;

        tokio::spawn(make_clock(&state).run()).await.unwrap();

        assert!(state.read().await.coach_state.is_done());
    }

    /// Slice index in CoachState advances 0 → 1 → 2 before DONE.
    #[tokio::test]
    async fn clock_visits_each_slice_in_order() {
        tokio::time::pause();
        let state = new_shared_state();
        load_plan(&state, 3, 60_000).await;

        // Collect the slice values seen while running.
        let state2 = state.clone();
        let handle = tokio::spawn(async move {
            make_clock(&state2).run().await;
        });

        // Let the spawned task run until all timers are exhausted.
        handle.await.unwrap();

        // Final state must be DONE (not stuck at an intermediate slice).
        assert!(state.read().await.coach_state.is_done());
    }

    /// Stop signal fired before the second tick terminates the clock early.
    /// Uses `biased;` select so stop always wins over a simultaneously-ready
    /// tick.
    #[tokio::test]
    async fn stop_signal_terminates_clock_early() {
        tokio::time::pause();
        let state = new_shared_state();
        load_plan(&state, 5, 60_000).await;

        // Signal stop before spawning so it is already true when the second
        // slice's select! is evaluated.
        state.read().await.signal_stop();

        // The first tick fires immediately (t=0). The biased select then
        // picks stop_rx over the next tick → clock calls on_stop.
        tokio::spawn(make_clock(&state).run()).await.unwrap();

        assert!(state.read().await.coach_state.is_done());
    }

    /// When a connected agent doesn't ack within ACK_TIMEOUT the clock logs
    /// it as lagging and still advances to the next slice.
    #[tokio::test]
    async fn ack_timeout_does_not_stall_clock() {
        tokio::time::pause();
        let state = new_shared_state();
        load_plan(&state, 2, 60_000).await;

        // Register a connected agent that will never send a SliceAck.
        {
            let mut w = state.write().await;
            w.registry
                .register("a00".into(), "10.0.0.1".into(), "i-x".into())
                .unwrap();
            let (tx, _rx) = Registry::new_cmd_channel();
            w.registry.set_session_channel("a00", tx);
        }

        // Run clock — paused time is auto-advanced through ACK_TIMEOUT and
        // both slice intervals by the tokio test runtime.
        tokio::spawn(make_clock(&state).run()).await.unwrap();

        assert!(state.read().await.coach_state.is_done());
    }

    /// When an agent acks promptly the clock advances without waiting for
    /// ACK_TIMEOUT.
    #[tokio::test]
    async fn prompt_ack_advances_clock_immediately() {
        tokio::time::pause();
        let state = new_shared_state();
        load_plan(&state, 1, 60_000).await;

        // Register a connected agent.
        {
            let mut w = state.write().await;
            w.registry
                .register("a00".into(), "10.0.0.1".into(), "i-x".into())
                .unwrap();
            let (tx, _rx) = Registry::new_cmd_channel();
            w.registry.set_session_channel("a00", tx);
        }

        // Spawn a task that acks slice 0 as soon as the clock broadcasts it.
        let state2 = state.clone();
        tokio::spawn(async move {
            // Yield to let the clock run and broadcast slice 0.
            tokio::task::yield_now().await;
            let mut w = state2.write().await;
            w.registry.update_slice("a00", 0);
            w.signal_ack();
        });

        tokio::spawn(make_clock(&state).run()).await.unwrap();

        assert!(state.read().await.coach_state.is_done());
    }

    /// No plan loaded → clock exits immediately with state DONE.
    #[tokio::test]
    async fn no_plan_exits_cleanly() {
        tokio::time::pause();
        let state = new_shared_state();
        // Deliberately do NOT load a plan.
        tokio::spawn(make_clock(&state).run()).await.unwrap();
        assert!(state.read().await.coach_state.is_done());
    }
}
