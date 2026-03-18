use anyhow::{Context, Result};
use std::fmt;
use tokio::signal::unix::{signal, SignalKind};

/// Represents an exit signal received by the application.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitSignal {
    Term,
    Int,
    Hup,
}

impl fmt::Display for ExitSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Term => write!(f, "SIG_TERM"),
            Self::Int => write!(f, "SIG_INT"),
            Self::Hup => write!(f, "SIG_HUP"),
        }
    }
}

/// Listen for termination, interrupt, and hangup signals.
/// 
/// Returns the first signal received among SIGTERM, SIGINT, or SIGHUP.
pub async fn listen_for_signals() -> Result<ExitSignal> {
    let mut term_sig = signal(SignalKind::terminate())
        .context("could not listen for TERM signals")?;
    let mut int_sig = signal(SignalKind::interrupt())
        .context("Could not listen for INT signal")?;
    let mut hup_sig = signal(SignalKind::hangup())
        .context("Could not listen for HUP signal")?;

    let sig = tokio::select! {
        Some(_signal) = term_sig.recv() => ExitSignal::Term,
        Some(_signal) = int_sig.recv() => ExitSignal::Int,
        Some(_signal) = hup_sig.recv() => ExitSignal::Hup,
    };
    
    Ok(sig)
}
