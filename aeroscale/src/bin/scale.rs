use aerocore::{asg, fetch_imds_credentials};
use anyhow::Result;
use clap::{Parser, Subcommand};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "scale")]
#[command(about = "Manage the FTP backend Auto Scaling Group")]
struct Args {
    /// AWS region
    #[arg(long, global = true, default_value = "eu-west-2")]
    region: String,

    /// Auto Scaling Group name
    #[arg(long, global = true, default_value = "ftp-asg")]
    asg_name: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show current ASG state: desired/min/max capacity and all running instances
    List,

    /// Set the desired number of running FTP backends
    Scale {
        /// Target number of instances (0–20)
        #[arg(long)]
        desired: u32,
    },

    /// Terminate one specific instance and decrement desired capacity.
    /// Use after draining the backend in keepalived.
    Terminate {
        /// EC2 instance ID to terminate (e.g. i-0abc1234567890def)
        #[arg(long)]
        instance_id: String,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let creds = fetch_imds_credentials().await?;

    match args.command {
        Command::List => cmd_list(&args.region, &args.asg_name, &creds).await,
        Command::Scale { desired } => {
            cmd_scale(&args.region, &args.asg_name, desired, &creds).await
        }
        Command::Terminate { instance_id } => {
            cmd_terminate(&args.region, &args.asg_name, &instance_id, &creds).await
        }
    }
}

// ── Subcommand implementations ────────────────────────────────────────────────

async fn cmd_list(region: &str, asg_name: &str, creds: &aerocore::AwsCredentials) -> Result<()> {
    let groups = asg::describe(region, asg_name, creds).await?;

    if groups.is_empty() {
        println!("No auto scaling groups found (has the ASG been created in the console yet?)");
        return Ok(());
    }

    for group in &groups {
        println!("\nAuto Scaling Group: {}", group.name);
        println!(
            "  Capacity — desired: {}  min: {}  max: {}",
            group.desired_capacity, group.min_size, group.max_size
        );
        println!("  Instances ({}):", group.instances.len());
        if group.instances.is_empty() {
            println!("    (none)");
        } else {
            println!("    {:<25} {:<15} {:<14} {}", "Instance ID", "Health", "State", "AZ");
            println!("    {}", "-".repeat(72));
            for inst in &group.instances {
                println!(
                    "    {:<25} {:<15} {:<14} {}",
                    inst.instance_id,
                    inst.health_status,
                    inst.lifecycle_state,
                    inst.availability_zone
                );
            }
        }
    }

    Ok(())
}

async fn cmd_scale(
    region: &str,
    asg_name: &str,
    desired: u32,
    creds: &aerocore::AwsCredentials,
) -> Result<()> {
    println!("📐 Setting desired capacity of '{asg_name}' to {desired} ...");
    asg::set_desired(region, asg_name, desired, creds).await?;
    println!("✅ Desired capacity set to {desired}.");
    Ok(())
}

async fn cmd_terminate(
    region: &str,
    asg_name: &str,
    instance_id: &str,
    creds: &aerocore::AwsCredentials,
) -> Result<()> {
    println!("🛑 Terminating instance '{instance_id}' in ASG '{asg_name}' ...");
    println!("   DesiredCapacity will be decremented by 1 automatically.");
    asg::terminate_instance(region, instance_id, creds, /*decrement=*/true).await?;
    println!("✅ Termination request accepted.");
    println!("   Run 'scale list' to watch the instance leave the group.");
    Ok(())
}
