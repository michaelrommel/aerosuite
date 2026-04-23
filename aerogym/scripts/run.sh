#!/bin/sh

# Fetch the metadata JSON
METADATA=$(curl -s "$ECS_CONTAINER_METADATA_URI_V4/task")

# Extract the Task ARN (requires 'jq')
TASK_ARN=$(echo $METADATA | jq -r '.TaskARN')

# Extract just the ID (the GUID at the end of the ARN)
TASK_ID=$(echo $TASK_ARN | awk -F/ '{print $NF}')

AEROGYM_INSTANCE_ID=${TASK_ID}
AEROGYM_PRIVATE_IP=$(ip a show dev eth1 | grep "inet " | awk '{print $2}')

export AEROGYM_INSTANCE_ID
export AEROGYM_PRIVATE_IP

if [ ! "${AEROGYM_INTERACTIVE}" = "true" ]; then
	./aerogym
	echo Sleeping now
fi
sleep infinity
