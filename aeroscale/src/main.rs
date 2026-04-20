use aeroscaler::{aws_query, extract_balanced, extract_scalar, fetch_imds_credentials, AwsCredentials};
use anyhow::Result;
use clap::{Parser, Subcommand};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "aeroscaler")]
#[command(about = "Manage the FTP backend Auto Scaling Group")]
struct Args {
    /// AWS region
    #[arg(long, global = true, default_value = "${REGION}")]
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

async fn cmd_list(region: &str, asg_name: &str, creds: &AwsCredentials) -> Result<()> {
    let host = format!("autoscaling.{region}.amazonaws.com");
    let xml = aws_query(
        &host,
        "autoscaling",
        region,
        creds,
        &[
            ("Action", "DescribeAutoScalingGroups"),
            ("Version", "2011-01-01"),
            ("AutoScalingGroupNames.member.1", asg_name),
        ],
    )
    .await?;

    let groups = parse_asg_describe(&xml)?;

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
                    inst.instance_id, inst.health_status, inst.lifecycle_state, inst.availability_zone
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
    creds: &AwsCredentials,
) -> Result<()> {
    println!("📐 Setting desired capacity of '{asg_name}' to {desired} ...");

    let host = format!("autoscaling.{region}.amazonaws.com");
    let desired_str = desired.to_string();
    let xml = aws_query(
        &host,
        "autoscaling",
        region,
        creds,
        &[
            ("Action", "SetDesiredCapacity"),
            ("Version", "2011-01-01"),
            ("AutoScalingGroupName", asg_name),
            ("DesiredCapacity", &desired_str),
            ("HonorCooldown", "false"),
        ],
    )
    .await?;

    if xml.contains("SetDesiredCapacityResponse") {
        println!("✅ Desired capacity set to {desired}.");
    } else {
        anyhow::bail!("Unexpected response from SetDesiredCapacity:\n{xml}");
    }

    Ok(())
}

async fn cmd_terminate(
    region: &str,
    asg_name: &str,
    instance_id: &str,
    creds: &AwsCredentials,
) -> Result<()> {
    println!("🛑 Terminating instance '{instance_id}' in ASG '{asg_name}' ...");
    println!("   DesiredCapacity will be decremented by 1 automatically.");

    let host = format!("autoscaling.{region}.amazonaws.com");
    let xml = aws_query(
        &host,
        "autoscaling",
        region,
        creds,
        &[
            ("Action", "TerminateInstanceInAutoScalingGroup"),
            ("Version", "2011-01-01"),
            ("InstanceId", instance_id),
            ("ShouldDecrementDesiredCapacity", "true"),
        ],
    )
    .await?;

    if xml.contains("TerminateInstanceInAutoScalingGroupResponse") {
        println!("✅ Termination request accepted.");
        println!("   Run 'aeroscaler list' to watch the instance leave the group.");
    } else {
        anyhow::bail!("Unexpected response from TerminateInstanceInAutoScalingGroup:\n{xml}");
    }

    Ok(())
}

// ── ASG XML parser ────────────────────────────────────────────────────────────

struct AsgGroup {
    name: String,
    desired_capacity: i64,
    min_size: i64,
    max_size: i64,
    instances: Vec<AsgInstance>,
}

struct AsgInstance {
    instance_id: String,
    availability_zone: String,
    lifecycle_state: String,
    health_status: String,
}

fn parse_asg_describe(xml: &str) -> Result<Vec<AsgGroup>> {
    fn parse_i64(haystack: &str, tag: &str) -> i64 {
        extract_scalar(haystack, tag)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    let mut groups = Vec::new();
    let mut remaining = xml;

    while let Some(group_xml) = extract_balanced(remaining, "member") {
        let skip = remaining
            .find("<member>")
            .map(|i| i + "<member>".len() + group_xml.len() + "</member>".len())
            .unwrap_or(remaining.len());
        remaining = &remaining[skip..];

        // Skip inner <member> blocks (e.g. AvailabilityZone strings) that
        // have no AutoScalingGroupName.
        let name = match extract_scalar(group_xml, "AutoScalingGroupName") {
            Some(n) => n.to_string(),
            None => continue,
        };

        let mut instances = Vec::new();
        if let Some(instances_block) = extract_balanced(group_xml, "Instances") {
            let mut inst_rem = instances_block;
            while let Some(inst_xml) = extract_balanced(inst_rem, "member") {
                let skip = inst_rem
                    .find("<member>")
                    .map(|i| i + "<member>".len() + inst_xml.len() + "</member>".len())
                    .unwrap_or(inst_rem.len());
                inst_rem = &inst_rem[skip..];

                instances.push(AsgInstance {
                    instance_id: extract_scalar(inst_xml, "InstanceId")
                        .unwrap_or("(unknown)")
                        .to_string(),
                    availability_zone: extract_scalar(inst_xml, "AvailabilityZone")
                        .unwrap_or("")
                        .to_string(),
                    lifecycle_state: extract_scalar(inst_xml, "LifecycleState")
                        .unwrap_or("")
                        .to_string(),
                    health_status: extract_scalar(inst_xml, "HealthStatus")
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }

        groups.push(AsgGroup {
            name,
            desired_capacity: parse_i64(group_xml, "DesiredCapacity"),
            min_size: parse_i64(group_xml, "MinSize"),
            max_size: parse_i64(group_xml, "MaxSize"),
            instances,
        });
    }

    Ok(groups)
}
