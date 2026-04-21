//! aeroplug — network connectivity facilitator.
//!
//! Consolidates ENI and secondary-IP management into a single binary.
//!
//! Subcommands:
//!   aeroplug ip   — assign or unassign a secondary private IP on an ENI
//!   aeroplug eni  — attach/detach/takeover an ENI by ID, name, tag, slot, or description

use anyhow::Result;
use clap::{Parser, Subcommand};

mod assign_ip;
mod attach;

#[derive(Parser)]
#[command(name = "aeroplug")]
#[command(about = "Network connectivity facilitator — manage ENIs and secondary IPs")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Assign or unassign a secondary private IP address on an ENI
    Ip(assign_ip::Args),
    /// Attach, detach, or take over an ENI (lookup by ID, name, description, tag, or slot)
    Eni(attach::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Ip(a)  => assign_ip::run(a).await,
        Command::Eni(a) => attach::run(a).await,
    }
}
