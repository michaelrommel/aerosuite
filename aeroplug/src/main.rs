//! aeroplug — network connectivity facilitator.
//!
//! Consolidates ENI and secondary-IP management into a single binary.
//!
//! Subcommands:
//!   aeroplug assign-ip   — assign or unassign a secondary private IP on an ENI
//!   aeroplug attach-eni  — attach/detach/takeover an ENI by ID, name, tag, or description
//!   aeroplug manage-eni  — slot-based ENI attach/detach (looks up ENI by aeroftp-slot tag)

use anyhow::Result;
use clap::{Parser, Subcommand};

mod assign_ip;
mod attach;
mod manage;

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
    AssignIp(assign_ip::Args),
    /// Attach, detach, or take over an ENI (lookup by ID, name, description, or tag)
    AttachEni(attach::Args),
    /// Slot-based ENI attach/detach (looks up ENI via aeroftp-slot tag)
    ManageEni(manage::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::AssignIp(a)  => assign_ip::run(a).await,
        Command::AttachEni(a) => attach::run(a).await,
        Command::ManageEni(a) => manage::run(a).await,
    }
}
