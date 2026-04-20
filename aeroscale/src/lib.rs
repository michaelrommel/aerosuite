//! aeroscale library — re-exports aerocore and exposes the aeroscale daemon's
//! internal modules to its own binaries.

pub use aerocore::*;

pub mod cleanup;
pub mod listener;
pub mod metrics;
pub mod scaler;
pub mod slot_network;
pub mod snapshot;
pub mod vrrp;
pub mod weight_sync;
