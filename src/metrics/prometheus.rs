use anyhow::{Context, Result};
use prometheus::{Encoder, TextEncoder};

/// Gather and encode Prometheus metrics as text format.
///
/// # Returns
/// A `Result` containing the encoded metrics buffer, or an error if encoding fails.
pub fn gather() -> Result<Vec<u8>> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = vec![];
    encoder
        .encode(&metric_families, &mut buffer)
        .context("Failed to encode metrics")?;
    Ok(buffer)
}
