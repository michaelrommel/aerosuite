//! aws-config — write key instance metadata to a VAR=VALUE config file.
//!
//! Reads instance-id, public IPv4, and private IPv4 from IMDSv2 and writes
//! them to /var/run/slotmanager/aws.conf (or a path given via --out).
//!
//! Intended to run once at boot (e.g. as an OpenRC start step or cloud-init
//! script) so that other services can source the file without hitting IMDS
//! themselves.

use aerocore::{fetch_imds_path, fetch_imds_token};
use anyhow::{Context, Result};
use clap::Parser;
use std::{fs, path::PathBuf};

#[derive(Parser)]
#[command(name = "aws-config")]
#[command(about = "Dump EC2 instance metadata to a VAR=VALUE config file")]
struct Args {
    /// Destination file (created if absent, overwritten if present)
    #[arg(long, default_value = "/var/run/aeroslot/aws.conf")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // One token, three metadata lookups
    let token = fetch_imds_token().await?;

    let instance_id = fetch_imds_path(&token, "instance-id").await?;
    let public_ipv4 = fetch_imds_path(&token, "public-ipv4").await?;
    let private_ipv4 = fetch_imds_path(&token, "local-ipv4").await?;

    let content = format!(
        "INSTANCE_ID={instance_id}\nPUBLIC_IPV4={public_ipv4}\nPRIVATE_IPV4={private_ipv4}\n"
    );

    if let Some(dir) = args.out.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("Cannot create directory {}", dir.display()))?;
    }

    fs::write(&args.out, &content)
        .with_context(|| format!("Cannot write {}", args.out.display()))?;

    // println!("Wrote {}:", args.out.display());
    // print!("{content}");

    Ok(())
}
