#!/bin/sh

# get cpu count from the metadata service
VCPUS=$(wget -q -O - "${ECS_CONTAINER_METADATA_URI_V4}/task" | jq '.Limits.CPU')

echo "Date,CPU,Memory,Connections"

while true; do
	T1_USAGE=$(cat /sys/fs/cgroup/cpuacct/cpuacct.usage)
	T1_TIME=$(date +%s%N)
	sleep 1

	T2_USAGE=$(cat /sys/fs/cgroup/cpuacct/cpuacct.usage)
	T2_TIME=$(date +%s%N)
	USAGE_DELTA=$((T2_USAGE - T1_USAGE))
	TIME_DELTA=$((T2_TIME - T1_TIME))
	CPU=$(echo "scale=2; (($USAGE_DELTA / $TIME_DELTA) / $VCPUS) * 100" | bc -l)

	CONN=$(wget -q -O - http://localhost:9090/metrics | grep '^ftp_sessions_total' | sed -e 's/ftp_sessions_total //')
	CONN=${CONN:-"0"}
	MEM=$(echo "$(cat /sys/fs/cgroup/memory/memory.usage_in_bytes) / 1024 / 1024" | bc)

	NOW=$(date +%s)

	echo "$NOW,$CPU,$MEM,$CONN"
done
