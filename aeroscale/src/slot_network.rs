//! Slot → IP address mapping — re-exported from aerocore.
//!
//! The implementation lives in `aerocore::slot_network` so that both
//! `aeroscale` (runtime scale decisions) and `aeropulse` (keepalived config
//! generation) share the exact same formula without duplication.

pub use aerocore::slot_network::{SlotNetwork};
