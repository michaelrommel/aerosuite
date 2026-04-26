//! Connected-agent registry.
//!
//! [`Registry`] tracks every agent that has called `Register`, maintains the
//! per-agent [`mpsc`] command channel opened when the agent calls `Session`,
//! and provides helpers for broadcasting [`CoachCommand`]s to all connected
//! agents.
//!
//! The registry lives inside [`crate::state::AppState`] behind a
//! `tokio::sync::RwLock`.  Reads acquire a shared lock; writes acquire an
//! exclusive lock.  Command-channel sends are deliberately **synchronous**
//! (`try_send`) so they never block while the write lock is held.

use std::collections::HashMap;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use aeroproto::aeromonitor::CoachCommand;

#[cfg(test)]
/// Capacity of each per-agent command channel (used only by test helpers).
const CMD_CHANNEL_CAPACITY: usize = 64;

// ── AgentEntry ────────────────────────────────────────────────────────────

/// All known state for one registered agent.
#[derive(Debug)]
pub struct AgentEntry {
    /// Human-readable ID supplied by the agent, e.g. `"a03"`.
    pub agent_id: String,

    /// Index assigned by aerocoach at registration time (0-based).
    pub agent_index: u32,

    /// Private IP reported by the agent from the ECS metadata endpoint.
    pub private_ip: String,

    /// ECS task short ID or EC2 instance ID.
    pub instance_id: String,

    /// Slice index the agent last acknowledged.
    pub current_slice: u32,

    /// Number of active FTP transfers as of the last MetricsUpdate.
    pub active_connections: u32,

    /// True once the agent has sent a [`PlanAck`] after the last Confirm.
    /// Reset to `false` at the start of each new Confirm broadcast so the UI
    /// always reflects the current round of confirmations.
    pub plan_acked: bool,

    /// True while the agent has an open `Session` stream.
    pub connected: bool,

    /// Monotonically increasing counter, incremented each time a new `Session`
    /// is attached.  Used by [`Registry::close_session`] to avoid stale
    /// cleanup tasks clobbering a newer session.
    pub session_gen: u64,

    /// Sender half of the per-agent command channel.
    /// Present once the agent calls `Session`; `None` before that or after
    /// the session ends.
    pub cmd_tx: Option<mpsc::Sender<CoachCommand>>,
}

impl AgentEntry {
    fn new(
        agent_id: String,
        agent_index: u32,
        private_ip: String,
        instance_id: String,
    ) -> Self {
        Self {
            agent_id,
            agent_index,
            private_ip,
            instance_id,
            current_slice:      0,
            active_connections: 0,
            connected:          false,
            plan_acked:         false,
            session_gen:        0,
            cmd_tx:             None,
        }
    }
}

// ── Lightweight snapshot (no proto dependency) ────────────────────────────

/// A plain-data snapshot of one agent's status, used by the HTTP /status
/// endpoint and the delta engine.
#[derive(Debug, Clone)]
pub struct AgentStatus {
    pub agent_id:           String,
    pub agent_index:        u32,
    pub private_ip:         String,
    pub instance_id:        String,
    pub current_slice:      u32,
    pub active_connections: u32,
    pub connected:          bool,
    pub plan_acked:         bool,
}

impl From<&AgentEntry> for AgentStatus {
    fn from(e: &AgentEntry) -> Self {
        Self {
            agent_id:           e.agent_id.clone(),
            agent_index:        e.agent_index,
            private_ip:         e.private_ip.clone(),
            instance_id:        e.instance_id.clone(),
            current_slice:      e.current_slice,
            active_connections: e.active_connections,
            connected:          e.connected,
            plan_acked:         e.plan_acked,
        }
    }
}

// ── Registry ──────────────────────────────────────────────────────────────

/// Registry of all agents that have called `Register`.
#[derive(Debug, Default)]
pub struct Registry {
    /// All known agents, keyed by agent_id.
    entries: HashMap<String, AgentEntry>,

    /// Monotonically increasing index assigned to the next new agent.
    next_index: u32,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Registration ──────────────────────────────────────────────────────

    /// Register a new agent and return its assigned index.
    ///
    /// If `agent_id` is already registered and *connected*, the registration
    /// is rejected.  If it is registered but *disconnected* (e.g. after a
    /// crash and restart), the existing entry is reused with the same index.
    ///
    /// # Errors
    /// Returns `Err` if the agent_id is already actively connected.
    pub fn register(
        &mut self,
        agent_id: String,
        private_ip: String,
        instance_id: String,
    ) -> Result<u32, String> {
        if let Some(entry) = self.entries.get(&agent_id) {
            if entry.connected {
                return Err(format!(
                    "agent {:?} is already registered and connected",
                    agent_id
                ));
            }
            // Disconnected re-registration: reuse the existing index.
            let index = entry.agent_index;
            let entry = self.entries.get_mut(&agent_id).unwrap();
            entry.private_ip = private_ip;
            entry.instance_id = instance_id;
            entry.current_slice = 0;
            entry.active_connections = 0;
            entry.plan_acked = false;
            entry.cmd_tx = None;
            info!(agent_id = %agent_id, index, "agent re-registered");
            return Ok(index);
        }

        let index = self.next_index;
        self.next_index += 1;
        self.entries.insert(
            agent_id.clone(),
            AgentEntry::new(agent_id.clone(), index, private_ip, instance_id),
        );
        info!(agent_id = %agent_id, index, "agent registered");
        Ok(index)
    }

    /// Attach a command channel to an already-registered agent.
    ///
    /// Increments the entry's `session_gen` counter and returns the new value.
    /// The caller (the session receive task) must pass this generation back to
    /// [`Self::close_session`] so that a stale cleanup from an old session
    /// cannot accidentally close a newer one.
    pub fn set_session_channel(
        &mut self,
        agent_id: &str,
        tx: mpsc::Sender<CoachCommand>,
    ) -> u64 {
        if let Some(entry) = self.entries.get_mut(agent_id) {
            entry.session_gen += 1;
            let new_gen = entry.session_gen;
            entry.cmd_tx = Some(tx);
            entry.connected = true;
            entry.current_slice = 0;
            entry.active_connections = 0;
            info!(agent_id = %agent_id, session_gen = new_gen, "agent session opened");
            new_gen
        } else {
            warn!(agent_id = %agent_id, "set_session_channel: agent not found");
            0
        }
    }

    /// Close a session only if `gen` still matches the entry's current
    /// `session_gen`.  This prevents a stale session-receive task from
    /// accidentally dropping the command channel of a *newer* session that
    /// the agent opened after reconnecting.
    pub fn close_session(&mut self, agent_id: &str, session_gen: u64) {
        if let Some(entry) = self.entries.get_mut(agent_id) {
            if entry.session_gen == session_gen {
                entry.connected = false;
                entry.cmd_tx = None;
                info!(agent_id = %agent_id, session_gen, "agent session closed");
            } else {
                debug!(
                    agent_id = %agent_id,
                    stale_gen  = session_gen,
                    current_gen = entry.session_gen,
                    "ignoring stale session close"
                );
            }
        }
    }

    /// Mark an agent as disconnected and drop its command channel,
    /// unconditionally.  Used by [`Self::broadcast`] when a channel is found
    /// to be closed.
    pub fn deregister(&mut self, agent_id: &str) {
        if let Some(entry) = self.entries.get_mut(agent_id) {
            entry.connected = false;
            entry.cmd_tx = None;
            info!(agent_id = %agent_id, "agent session closed");
        }
    }

    // ── Plan-ack tracking ─────────────────────────────────────────────────

    /// Mark one agent as having acknowledged the latest plan confirm.
    pub fn ack_plan(&mut self, agent_id: &str) {
        if let Some(entry) = self.entries.get_mut(agent_id) {
            entry.plan_acked = true;
        }
    }

    /// Reset all ack flags at the start of a new Confirm broadcast.
    pub fn reset_plan_acks(&mut self) {
        for entry in self.entries.values_mut() {
            entry.plan_acked = false;
        }
    }

    /// How many agents have sent a PlanAck since the last Confirm.
    pub fn plan_ack_count(&self) -> usize {
        self.entries.values().filter(|e| e.plan_acked).count()
    }

    // ── State updates (called from the Session receive task) ──────────────

    /// Record that an agent has acknowledged a slice tick.
    pub fn update_slice(&mut self, agent_id: &str, slice_index: u32) {
        if let Some(entry) = self.entries.get_mut(agent_id) {
            entry.current_slice = slice_index;
            debug!(agent_id = %agent_id, slice = slice_index, "slice ack recorded");
        }
    }

    /// Update the active-connection count from a MetricsUpdate.
    pub fn update_active_connections(&mut self, agent_id: &str, count: u32) {
        if let Some(entry) = self.entries.get_mut(agent_id) {
            entry.active_connections = count;
        }
    }

    // ── Querying ──────────────────────────────────────────────────────────

    /// Returns `true` if the agent_id has been registered (connected or not).
    pub fn contains(&self, agent_id: &str) -> bool {
        self.entries.contains_key(agent_id)
    }

    /// Total number of registered agents (connected or not).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of agents with an active Session stream.
    pub fn connected_count(&self) -> usize {
        self.entries.values().filter(|e| e.connected).count()
    }

    /// Returns `true` when every connected agent has acked `slice_index`.
    ///
    /// Agents that are registered but not connected are ignored — they cannot
    /// advance the slice anyway.
    pub fn all_acked(&self, slice_index: u32) -> bool {
        self.entries
            .values()
            .filter(|e| e.connected)
            .all(|e| e.current_slice >= slice_index)
    }

    /// Snapshot of all agent statuses (for HTTP /status and the delta engine).
    pub fn status_snapshot(&self) -> Vec<AgentStatus> {
        let mut snapshot: Vec<AgentStatus> = self.entries.values().map(AgentStatus::from).collect();
        // Stable order by agent_index for deterministic output.
        snapshot.sort_by_key(|s| s.agent_index);
        snapshot
    }

    // ── Command dispatch ──────────────────────────────────────────────────

    /// Attempt to deliver `cmd` to every connected agent.
    ///
    /// Uses `try_send` so this method is synchronous and can be called while
    /// holding the [`AppState`] write lock.  Agents with a full channel are
    /// skipped with a warning (they are lagging).  Agents whose channel has
    /// been dropped are marked as disconnected.
    ///
    /// Returns the number of agents successfully sent to.
    pub fn broadcast(&mut self, cmd: CoachCommand) -> usize {
        let ids: Vec<String> = self.entries.keys().cloned().collect();
        let mut sent = 0;
        let mut disconnected = Vec::new();

        for id in &ids {
            let Some(entry) = self.entries.get(id) else {
                continue;
            };
            if !entry.connected {
                continue;
            }
            let Some(ref tx) = entry.cmd_tx else {
                continue;
            };
            match tx.try_send(cmd.clone()) {
                Ok(()) => sent += 1,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(agent_id = %id, "command channel full — agent is lagging, skipping tick");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    disconnected.push(id.clone());
                }
            }
        }

        for id in disconnected {
            self.deregister(&id);
        }

        sent
    }

    /// Create a new bounded command channel for one agent and return both
    /// halves.  The caller is responsible for storing the sender via
    /// [`Self::set_session_channel`].
    #[cfg(test)]
    pub fn new_cmd_channel() -> (mpsc::Sender<CoachCommand>, mpsc::Receiver<CoachCommand>) {
        mpsc::channel(CMD_CHANNEL_CAPACITY)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registry {
        Registry::new()
    }

    fn entry(r: &mut Registry, id: &str) -> u32 {
        r.register(id.into(), "10.0.0.1".into(), "i-abc".into())
            .expect("register failed")
    }

    #[test]
    fn register_assigns_sequential_indices() {
        let mut r = reg();
        assert_eq!(entry(&mut r, "a00"), 0);
        assert_eq!(entry(&mut r, "a01"), 1);
        assert_eq!(entry(&mut r, "a02"), 2);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn duplicate_connected_registration_rejected() {
        let mut r = reg();
        entry(&mut r, "a00");
        let (tx, _rx) = Registry::new_cmd_channel();
        r.set_session_channel("a00", tx);
        assert!(r.register("a00".into(), "10.0.0.1".into(), "i-x".into()).is_err());
    }

    #[test]
    fn disconnected_agent_may_re_register() {
        let mut r = reg();
        let original_index = entry(&mut r, "a00");
        // Simulate connection + disconnection
        let (tx, _rx) = Registry::new_cmd_channel();
        r.set_session_channel("a00", tx);
        r.deregister("a00");
        // Re-register: should get the same index back
        let new_index = r
            .register("a00".into(), "10.0.0.2".into(), "i-new".into())
            .unwrap();
        assert_eq!(new_index, original_index);
        assert_eq!(r.len(), 1); // still one entry
    }

    #[test]
    fn set_session_channel_marks_connected() {
        let mut r = reg();
        entry(&mut r, "a00");
        assert!(!r.entries["a00"].connected);
        let (tx, _rx) = Registry::new_cmd_channel();
        r.set_session_channel("a00", tx);
        assert!(r.entries["a00"].connected);
        assert_eq!(r.connected_count(), 1);
    }

    #[test]
    fn deregister_marks_disconnected_and_clears_channel() {
        let mut r = reg();
        entry(&mut r, "a00");
        let (tx, _rx) = Registry::new_cmd_channel();
        r.set_session_channel("a00", tx);
        r.deregister("a00");
        assert!(!r.entries["a00"].connected);
        assert!(r.entries["a00"].cmd_tx.is_none());
    }

    #[test]
    fn all_acked_true_when_all_agents_up_to_date() {
        let mut r = reg();
        entry(&mut r, "a00");
        entry(&mut r, "a01");
        let (tx0, _) = Registry::new_cmd_channel();
        let (tx1, _) = Registry::new_cmd_channel();
        r.set_session_channel("a00", tx0);
        r.set_session_channel("a01", tx1);
        r.update_slice("a00", 2);
        r.update_slice("a01", 2);
        assert!(r.all_acked(2));
    }

    #[test]
    fn all_acked_false_when_one_agent_behind() {
        let mut r = reg();
        entry(&mut r, "a00");
        entry(&mut r, "a01");
        let (tx0, _) = Registry::new_cmd_channel();
        let (tx1, _) = Registry::new_cmd_channel();
        r.set_session_channel("a00", tx0);
        r.set_session_channel("a01", tx1);
        r.update_slice("a00", 2);
        r.update_slice("a01", 1); // behind
        assert!(!r.all_acked(2));
    }

    #[test]
    fn all_acked_ignores_disconnected_agents() {
        let mut r = reg();
        entry(&mut r, "a00");
        entry(&mut r, "a01");
        let (tx0, _) = Registry::new_cmd_channel();
        r.set_session_channel("a00", tx0);
        r.update_slice("a00", 2);
        // a01 never connected — should not block ack check
        assert!(r.all_acked(2));
    }

    #[test]
    fn status_snapshot_sorted_by_index() {
        let mut r = reg();
        entry(&mut r, "a02");
        entry(&mut r, "a00");
        entry(&mut r, "a01");
        let snap = r.status_snapshot();
        let ids: Vec<&str> = snap.iter().map(|s| s.agent_id.as_str()).collect();
        assert_eq!(ids, ["a02", "a00", "a01"]);
    }
}
