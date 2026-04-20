//! Read Auto Scaling Group instance list and group-level capacity info from AWS.

use anyhow::Result;
use tracing::debug;

use aerocore::{asg, AwsCredentials};

use super::{AsgGroupInfo, AsgInstance};

/// Describe `asg_name` and return the group capacity info plus all instances
/// (any lifecycle state).  Sorted by instance-id for stable output.
pub async fn read_all(
    region:   &str,
    asg_name: &str,
    creds:    &AwsCredentials,
) -> Result<(Option<AsgGroupInfo>, Vec<AsgInstance>)> {
    let groups = asg::describe(region, asg_name, creds).await?;

    // In practice there is always exactly one group; take the first.
    let group_info = groups.first().map(|g| AsgGroupInfo {
        name:             g.name.clone(),
        desired_capacity: g.desired_capacity,
        min_size:         g.min_size,
        max_size:         g.max_size,
    });

    let mut instances: Vec<AsgInstance> = groups
        .into_iter()
        .flat_map(|g| g.instances)
        .map(|i| AsgInstance {
            instance_id:     i.instance_id,
            lifecycle_state: i.lifecycle_state,
            health_status:   i.health_status,
        })
        .collect();

    instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    debug!(
        "{} ASG instance(s), group info: {:?}",
        instances.len(),
        group_info.as_ref().map(|g| format!(
            "desired={} min={} max={}", g.desired_capacity, g.min_size, g.max_size
        ))
    );
    Ok((group_info, instances))
}
