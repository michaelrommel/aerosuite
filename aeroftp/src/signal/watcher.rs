use anyhow::{Context, Result};
use std::fmt;
use tokio::signal::unix::{signal, SignalKind};

/// Represents a Unix signal that determines application behavior.
///
/// This enum captures the three primary signals that AeroFTP responds to:
///   * `SIGTERM` - Request for graceful shutdown
///   * `SIGINT` - Interrupt signal (typically Ctrl+C), also triggers graceful shutdown
///   * `SIGHUP` - Hangup signal, requests application restart
///
/// # Usage Pattern
/// The typical usage pattern in AeroFTP is:
/// ```no_run
/// loop {
///     match listen_for_signals().await? {
///         ExitSignal::Hup => {
///             // Restart the application
///             continue;
///         }
///         _ => break, // Exit on TERM or INT
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitSignal {
    Term,
    Int,
    Hup,
}

impl fmt::Display for ExitSignal {
    /// Formats the signal as a human-readable string.
    ///
    /// # Examples
    /// ```
    /// use aeroftp::signal::ExitSignal;
    ///
    /// assert_eq!(format!("{}", ExitSignal::Term), "SIG_TERM");
    /// assert_eq!(format!("{}", ExitSignal::Int), "SIG_INT");
    /// assert_eq!(format!("{}", ExitSignal::Hup), "SIG_HUP");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Term => write!(f, "SIG_TERM"),
            Self::Int => write!(f, "SIG_INT"),
            Self::Hup => write!(f, "SIG_HUP"),
        }
    }
}

/// Listen for and return the first received Unix signal among SIGTERM, SIGINT, or SIGHUP.
///
/// This function sets up handlers for three critical signals that control application lifecycle:
/// * `SIGTERM` - Standard termination signal (graceful shutdown)
/// * `SIGINT` - Interrupt signal from terminal (Ctrl+C, graceful shutdown)
/// * `SIGHUP` - Hangup signal (typically used to request configuration reload or restart)
///
/// The function uses `tokio::select!` to race between the three signals and returns
/// whichever arrives first. This provides efficient single-signal handling without
/// polling overhead.
///
/// # Returns
/// * `Ok(ExitSignal::Term)` - Received SIGTERM (graceful shutdown requested)
/// * `Ok(ExitSignal::Int)` - Received SIGINT (Ctrl+C, graceful shutdown requested)
/// * `Ok(ExitSignal::Hup)` - Received SIGHUP (restart/reload requested)
/// # Errors
/// Returns an error if signal handlers cannot be installed due to system limitations,
/// such as running on a non-Unix platform or insufficient permissions.
///
/// # Examples
/// ```no_run
/// use aeroftp::signal;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let signal = signal::listen_for_signals().await?;
///     println!("Received signal: {}", signal);
///     
///     match signal {
///         signal::ExitSignal::Hup => {
///             // Restart application
///             println!("Restarting...");
///         }
///         _ => {
///             // Exit on TERM or INT
///             println!("Shutting down gracefully");
///         }
///     }
///     
///     Ok(())
/// }
/// ```
#[must_use = "signal handling result determines application behavior"]
pub async fn listen_for_signals() -> Result<ExitSignal> {
    let mut term_sig =
        signal(SignalKind::terminate()).context("could not listen for TERM signals")?;
    let mut int_sig = signal(SignalKind::interrupt()).context("could not listen for INT signal")?;
    let mut hup_sig = signal(SignalKind::hangup()).context("could not listen for HUP signal")?;

    let sig = tokio::select! {
        Some(_signal) = term_sig.recv() => ExitSignal::Term,
        Some(_signal) = int_sig.recv() => ExitSignal::Int,
        Some(_signal) = hup_sig.recv() => ExitSignal::Hup,
    };

    Ok(sig)
}
