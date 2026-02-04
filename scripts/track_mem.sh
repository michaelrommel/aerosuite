#!/bin/sh
while true; do
	echo "$(cat /sys/fs/cgroup/memory/memory.usage_in_bytes) / 1024 / 1024" | bc
	sleep 1
done
