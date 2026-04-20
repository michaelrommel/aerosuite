//! aerocore — shared library for the aerosuite workspace.
//!
//! Re-exports all public items from each sub-module so dependents can do
//! `use aerocore::AwsCredentials` without caring about the internal layout.

pub mod asg;
pub mod aws;
pub mod redis_pool;
pub mod slot_network;

pub use aws::{
    extract_all_scalars, extract_balanced, extract_scalar, fetch_imds_credentials,
    fetch_imds_instance_id, fetch_imds_path, fetch_imds_token, hmac_sha256, sha256_hex,
    sigv4_sign, AwsCredentials, SigV4Result,
};

// aws_query is also useful at the top level (most callers need it).
pub use aws::aws_query;

// SlotNetwork is shared by aeroscale (runtime decisions) and aeropulse
// (keepalived config generation) so it lives here rather than in either crate.
pub use slot_network::SlotNetwork;
