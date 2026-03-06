#!/bin/sh

# get cpu count from the metadata service
VCPUS=$(wget -q -O - "${ECS_CONTAINER_METADATA_URI_V4}/task" | jq '.Limits.CPU')

echo -e "\nTime,CPU,Memory,Connections"

START_TIME=$(date +%s%N)
CONNECTED=0
while true; do
	T1_USAGE=$(cat /sys/fs/cgroup/cpuacct/cpuacct.usage)
	T1_TIME=$(date +%s%N)
	sleep 1

	T2_USAGE=$(cat /sys/fs/cgroup/cpuacct/cpuacct.usage)
	T2_TIME=$(date +%s%N)
	USAGE_DELTA=$((T2_USAGE - T1_USAGE))
	TIME_DELTA=$((T2_TIME - T1_TIME))
	CPU=$(echo "scale=2; (($USAGE_DELTA / $TIME_DELTA) / $VCPUS) * 100" | bc -l)

	MEM=$(echo "$(cat /sys/fs/cgroup/memory/memory.usage_in_bytes) / 1024 / 1024" | bc)

	CONN=$(wget -q -O - http://localhost:9090/metrics | grep '^ftp_sessions_total' | sed -e 's/ftp_sessions_total //')
	CONN=${CONN:-"0"}
	if [ $CONNECTED -eq 0 ] && [ $CONN -gt 0 ]; then
		START_TIME=$(date +%s%N)
		CONNECTED=1
		echo -e "\nTime,CPU,Memory,Connections"
	fi

	ELAPSED=$(((T2_TIME - START_TIME) / 1000 / 1000))

	echo "$ELAPSED,$CPU,$MEM,$CONN"
done
