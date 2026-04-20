//! AWS Auto Scaling Group operations.
//!
//! Wraps the ASG Query API into typed, reusable functions that all aerosuite
//! binaries can call.  The CLI presentation (printing, exit codes) is left to
//! the caller.

use crate::aws::{aws_query, extract_balanced, extract_scalar, AwsCredentials};
use anyhow::Result;

// ── Types ─────────────────────────────────────────────────────────────────────

pub struct AsgGroup {
    pub name: String,
    pub desired_capacity: i64,
    pub min_size: i64,
    pub max_size: i64,
    pub instances: Vec<AsgInstance>,
}

pub struct AsgInstance {
    pub instance_id: String,
    pub availability_zone: String,
    pub lifecycle_state: String,
    pub health_status: String,
}

// ── API calls ─────────────────────────────────────────────────────────────────

/// Call `DescribeAutoScalingGroups` and return the parsed group list.
pub async fn describe(
    region: &str,
    asg_name: &str,
    creds: &AwsCredentials,
) -> Result<Vec<AsgGroup>> {
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
    parse_asg_describe(&xml)
}

/// Call `SetDesiredCapacity` on an ASG.
pub async fn set_desired(
    region: &str,
    asg_name: &str,
    desired: u32,
    creds: &AwsCredentials,
) -> Result<()> {
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
        Ok(())
    } else {
        anyhow::bail!("Unexpected response from SetDesiredCapacity:\n{xml}");
    }
}

/// Call `TerminateInstanceInAutoScalingGroup` and decrement desired capacity.
pub async fn terminate_instance(
    region: &str,
    instance_id: &str,
    creds: &AwsCredentials,
) -> Result<()> {
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
            ("ShouldDecrementDesiredCapacity", "false"),
        ],
    )
    .await?;

    if xml.contains("TerminateInstanceInAutoScalingGroupResponse") {
        Ok(())
    } else {
        anyhow::bail!("Unexpected response from TerminateInstanceInAutoScalingGroup:\n{xml}");
    }
}

// ── XML parser ────────────────────────────────────────────────────────────────

pub fn parse_asg_describe(xml: &str) -> Result<Vec<AsgGroup>> {
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
