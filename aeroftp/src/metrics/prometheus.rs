use anyhow::{Context, Result};
use prometheus::{Encoder, TextEncoder};

/// Gathers all registered Prometheus metrics and encodes them in text format.
///
/// This function retrieves all currently registered metric families from the global
/// registry and encodes them using the Prometheus text encoding specification.
/// The output is suitable for serving at an HTTP `/metrics` endpoint or writing to a file.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The encoded metrics in Prometheus text format
/// * `Err(anyhow::Error)` - If metric gathering or encoding fails
///
/// # Examples
/// ```no_run
/// use aeroftp::metrics;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let metrics_data = metrics::gather()?;
///     println!("Prometheus metrics:\n{}", String::from_utf8_lossy(&metrics_data));
///     Ok(())
/// }
/// ```
pub fn gather() -> Result<Vec<u8>> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = vec![];
    encoder
        .encode(&metric_families, &mut buffer)
        .context("failed to encode metrics")?;
    Ok(buffer)
}
