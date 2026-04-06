use anyhow::Context;
use libunftp::options::{ActivePassiveMode, PassiveHost};
use opendal::{services::S3, Operator};

use crate::aws::CachingAwsCredentialLoader;
use log::{debug, error, info};
use unftp_auth_jsonfile::JsonFileAuthenticator;
use unftp_sbe_opendal::OpendalStorage;

/// Type-safe wrapper for the passive port range used in FTP data connections.
///
/// This newtype ensures that port ranges are validated at construction time,
/// preventing configuration errors where start > end.
///
/// # Security
/// The port range is a well-known value (typically 30000-49999) and can be safely logged.
struct PassivePortRange {
    /// Start of the passive port range (inclusive).
    start: u16,
    /// End of the passive port range (inclusive).
    end: u16,
}

impl PassivePortRange {
    /// Creates a new passive port range with validation.
    ///
    /// This constructor validates that `start <= end` to prevent configuration errors.
    /// The validation ensures the port range is usable before it's applied to the server.
    ///
    /// # Arguments
    /// * `start` - The start of the passive port range (inclusive)
    /// * `end` - The end of the passive port range (inclusive)
    ///
    /// # Errors
    /// Returns an error if:
    /// * The start port is greater than the end port (`start > end`)
    ///
    /// # Examples
    /// ```
    /// use aeroftp::ftp::PassivePortRange;
    ///
    /// // Valid range
    /// let range = PassivePortRange::new(30000, 49999).unwrap();
    /// assert_eq!(range.get(), (30000, 49999));
    ///
    /// // Invalid range - start > end
    /// let invalid = PassivePortRange::new(50000, 30000);
    /// assert!(invalid.is_err());
    /// ```
    pub fn new(start: u16, end: u16) -> anyhow::Result<Self> {
        if start > end {
            anyhow::bail!(
                "Passive port range start ({}) cannot be greater than end ({})",
                start,
                end
            );
        }
        Ok(PassivePortRange { start, end })
    }

    /// Returns the underlying port range as a tuple.
    pub fn get(&self) -> (u16, u16) {
        (self.start, self.end)
    }
}

impl std::fmt::Display for PassivePortRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

// this would allow us to listen on IPv4 and IPv6 simultaneously
// however it creates problems with some dual stack clients
// since we only route IPv4 addresses today, this is a safe choice
// const FTP_ADDRESS: &str = "[::]";
const FTP_ADDRESS: &str = "0.0.0.0";
const CONTROL_PORT: u16 = 21;
const PASSIVE_PORT_RANGE_START: u16 = 30000;
const PASSIVE_PORT_RANGE_END: u16 = 49999;

/// Default FTP session idle timeout in seconds (10 minutes).
///
/// Sessions that remain idle for longer than this duration will be terminated.
/// This helps prevent resource exhaustion from zombie connections.
const DEFAULT_IDLE_SESSION_TIMEOUT_SECS: u64 = 600;

/// Grace period for graceful FTP server shutdown in seconds.
///
/// After receiving a shutdown signal, the server waits this duration to allow
/// active sessions to complete before forcibly closing connections. This value
/// is chosen to balance between giving users time to finish transfers and
/// not keeping zombie processes running indefinitely.
const DEFAULT_SHUTDOWN_GRACE_PERIOD_SECS: u64 = 10;

/// Starts the FTP server with AWS S3 backend storage.
///
/// This function initializes and starts an FTP server that:
/// * Listens on port 21 for incoming FTP connections
/// * Uses AWS S3 as the backend file storage via opendal
/// * Authenticates users from a JSON credentials file (`credentials.json`)
/// * Supports both active and passive FTP modes
/// * Integrates with Prometheus metrics collection
///
/// # Configuration
/// The server requires the following environment variables:
/// * `AWS_S3_REGION` - AWS region (e.g., "eu-west-2")
/// * `AWS_S3_BUCKET` - S3 bucket name (e.g., "my-ftp-bucket")
///
/// # Arguments
/// * `shutdown` - A broadcast receiver that signals when the server should shut down gracefully
///
/// # Returns
/// * `Ok(())` - FTP server started successfully and running in background task
/// # Errors
/// Returns an error if:
/// * The credentials file cannot be loaded or parsed
/// * The S3 operator fails to initialize
/// * Server builder configuration is invalid (e.g., port range issues)
///
/// # Examples
/// ```no_run
/// use aeroftp::ftp;
/// use tokio::sync::{broadcast};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let (shutdown_sender, shutdown_receiver) = broadcast::channel(1);
///     
///     // Start the FTP server
///     ftp::start_ftp(shutdown_receiver).await?;
///     
///     Ok(())
/// }
/// ```
#[must_use = "FTP server startup result indicates success or failure"]
pub async fn start_ftp(
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let caching_provider = CachingAwsCredentialLoader::default();

    let region = std::env::var("AWS_S3_REGION")?;
    let bucket = std::env::var("AWS_S3_BUCKET")?;

    let builder = S3::default()
        .customized_credential_load(Box::new(caching_provider))
        .endpoint("https://s3.amazonaws.com")
        .region(&region)
        .bucket(&bucket)
        .root("/");

    // Initialize the Operator
    let op: Operator = Operator::new(builder)?.finish();

    // Wrap the operator with `OpendalStorage`
    let backend = OpendalStorage::new(op);

    let authenticator = JsonFileAuthenticator::from_file("credentials.json")
        .map_err(|e| anyhow::anyhow!("could not load credentials file: {}", e))?;

    let passive_port_range =
        PassivePortRange::new(PASSIVE_PORT_RANGE_START, PASSIVE_PORT_RANGE_END)
            .context("Invalid passive port range configuration")?;

    let server = libunftp::ServerBuilder::new(Box::new(move || backend.clone()))
        .authenticator(std::sync::Arc::new(authenticator))
        .shutdown_indicator(async move {
            shutdown.recv().await.ok();
            debug!("shutting down FTP server");
            libunftp::options::Shutdown::new().grace_period(std::time::Duration::from_secs(
                DEFAULT_SHUTDOWN_GRACE_PERIOD_SECS,
            ))
        })
        .idle_session_timeout(DEFAULT_IDLE_SESSION_TIMEOUT_SECS)
        // .proxy_protocol_mode(CONTROL_PORT)
        .active_passive_mode(ActivePassiveMode::ActiveAndPassive)
        .passive_host(PassiveHost::FromConnection)
        .passive_ports(passive_port_range.get().0..=passive_port_range.get().1)
        .metrics()
        .build()?;

    tokio::spawn(async move {
        let addr = format!("{}:{}", FTP_ADDRESS, CONTROL_PORT);
        info!("starting ftp server on {}", &addr);
        if let Err(e) = server.listen(addr.clone()).await {
            error!("FTP server failed to listen on {}: {}", &addr, e);
        }
        debug!("FTP exiting");
    });

    Ok(())
}
