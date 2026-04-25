#!/bin/sh

# 1. Get the CPU Quota and Period
# Quota is the total allowed time (in microseconds) per period
# Period is usually 100,000 (100ms)
# QUOTA=$(cat /sys/fs/cgroup/cpu/cpu.cfs_quota_us)
# PERIOD=$(cat /sys/fs/cgroup/cpu/cpu.cfs_period_us)

# # Handle cases where no limit is set (-1)
# if [ "$QUOTA" -le 0 ]; then
# 	echo "No CPU limit detected. Script requires an ECS Task/Container limit."
# 	exit 1
# fi

# # Calculate number of vCPUs allocated (e.g., 512 units = 0.5 vCPUs)
# VCPUS=$(echo "scale=2; $QUOTA / $PERIOD" | bc -l)

# get it from the metadata service
VCPUS=$(wget -q -O - "${ECS_CONTAINER_METADATA_URI_V4}/task" | jq '.Limits.CPU')

echo "Detected Allocation: $VCPUS vCPUs"
echo "Monitoring CPU utilization... (Ctrl+C to stop)"

while true; do
	# 2. Capture initial CPU usage (in nanoseconds)
	T1_USAGE=$(cat /sys/fs/cgroup/cpuacct/cpuacct.usage)
	T1_TIME=$(date +%s%N)

	sleep 1

	# 3. Capture second CPU usage
	T2_USAGE=$(cat /sys/fs/cgroup/cpuacct/cpuacct.usage)
	T2_TIME=$(date +%s%N)

	# 4. Calculate deltas
	# (T2_USAGE - T1_USAGE) is nanoseconds used by the container
	# (T2_TIME - T1_TIME) is total elapsed nanoseconds
	USAGE_DELTA=$((T2_USAGE - T1_USAGE))
	TIME_DELTA=$((T2_TIME - T1_TIME))

	# 5. Calculate percentage relative to the allocated limit
	# Formula: (Usage_Delta / Time_Delta) / vCPUs * 100
	UTIL=$(echo "scale=2; (($USAGE_DELTA / $TIME_DELTA) / $VCPUS) * 100" | bc -l)

	echo "$UTIL"
done
