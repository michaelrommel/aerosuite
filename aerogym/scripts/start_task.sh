#!/usr/bin/env bash

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/zscaler_root.pem

TARGET=$1
if [[ -z "$TARGET" ]]; then
	echo "Usage: $0 <FTP container IP address>"
	exit 1
fi

SPOT=$2
if [[ -z "$SPOT" ]]; then
	SPOT=FARGATE_SPOT
fi

# AEROSTRESS_TARGET:	The IP Address to connect to
# AEROSTRESS_DELAY:		Ramp up delay in seconds between consecutive batches
# AEROSTRESS_BATCHES:	Number of batches to start
# AEROSTRESS_TASKS:		Number of tasks per batch, one task is one client
# AEROSTRESS_SIZE:		The file size in MBytes to use for uploads
# AEROSTRESS_LIMITER:	Boolean value, if a limit shall be set
# AEROSTRESS_CHUNK:		The size of the chunks in bytes to send the file with, 0 means use standard 4k
# AEROSTRESS_INTERVAL:	Interval in ms between chunks of data, 0 means, no rate limit
# AEROSTRESS_MSS:		The Maximum Segment Size of the socket, 0 means no fixed MSS

small_task() {
	aws ecs run-task \
		--no-cli-pager \
		--count 1 \
		--cluster aeroftp-cluster \
		--capacity-provider-strategy capacityProvider=${SPOT},weight=1 \
		--network-configuration "awsvpcConfiguration={subnets=[subnet-0cc4dd3ae05f9c278],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
		--task-definition aerogym:1 \
		--enable-execute-command \
		--overrides "{
			\"containerOverrides\": [{
				\"name\": \"aerogym\",
				\"environment\": [
					{\"name\": \"AEROSTRESS_TARGET\", \"value\": \"${TARGET}\"},
					{\"name\": \"AEROSTRESS_DELAY\", \"value\": \"120\"},
					{\"name\": \"AEROSTRESS_BATCHES\", \"value\": \"2\"},
					{\"name\": \"AEROSTRESS_TASKS\", \"value\": \"200\"},
					{\"name\": \"AEROSTRESS_SIZE\", \"value\": \"50\"},
					{\"name\": \"AEROSTRESS_LIMITER\", \"value\": \"true\"},
					{\"name\": \"AEROSTRESS_CHUNK\", \"value\": \"32768\"},
					{\"name\": \"AEROSTRESS_INTERVAL\", \"value\": \"20\"},
					{\"name\": \"AEROSTRESS_MSS\", \"value\": \"0\"}
				]
			}]
		}"
}

large_task() {
	aws ecs run-task \
		--no-cli-pager \
		--count 1 \
		--cluster aeroftp-cluster \
		--capacity-provider-strategy capacityProvider=${SPOT},weight=1 \
		--network-configuration "awsvpcConfiguration={subnets=[subnet-0cc4dd3ae05f9c278],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
		--task-definition aerogym:1 \
		--enable-execute-command \
		--overrides "{
			\"containerOverrides\": [{
				\"name\": \"aerogym\",
				\"environment\": [
					{\"name\": \"AEROSTRESS_TARGET\", \"value\": \"${TARGET}\"},
					{\"name\": \"AEROSTRESS_DELAY\", \"value\": \"30\"},
					{\"name\": \"AEROSTRESS_BATCHES\", \"value\": \"30\"},
					{\"name\": \"AEROSTRESS_TASKS\", \"value\": \"200\"},
					{\"name\": \"AEROSTRESS_SIZE\", \"value\": \"500\"},
					{\"name\": \"AEROSTRESS_LIMITER\", \"value\": \"true\"},
					{\"name\": \"AEROSTRESS_CHUNK\", \"value\": \"32768\"},
					{\"name\": \"AEROSTRESS_INTERVAL\", \"value\": \"10\"},
					{\"name\": \"AEROSTRESS_MSS\", \"value\": \"0\"}
				]
			}]
		}"
}

slow_task() {
	aws ecs run-task \
		--no-cli-pager \
		--count 1 \
		--cluster aeroftp-cluster \
		--capacity-provider-strategy capacityProvider=${SPOT},weight=1 \
		--network-configuration "awsvpcConfiguration={subnets=[subnet-0cc4dd3ae05f9c278],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
		--task-definition aerogym:1 \
		--enable-execute-command \
		--overrides "{
			\"containerOverrides\": [{
				\"name\": \"aerogym\",
				\"environment\": [
					{\"name\": \"AEROSTRESS_TARGET\", \"value\": \"${TARGET}\"},
					{\"name\": \"AEROSTRESS_DELAY\", \"value\": \"30\"},
					{\"name\": \"AEROSTRESS_BATCHES\", \"value\": \"1\"},
					{\"name\": \"AEROSTRESS_TASKS\", \"value\": \"1\"},
					{\"name\": \"AEROSTRESS_SIZE\", \"value\": \"20\"},
					{\"name\": \"AEROSTRESS_LIMITER\", \"value\": \"true\"},
					{\"name\": \"AEROSTRESS_CHUNK\", \"value\": \"4096\"},
					{\"name\": \"AEROSTRESS_INTERVAL\", \"value\": \"20\"},
					{\"name\": \"AEROSTRESS_MSS\", \"value\": \"0\"}
				]
			}]
		}"
}

for i in {1..5}; do
	small_task
	sleep 5
done

sleep 120

for i in {1..20}; do
	large_task
	sleep 5
done

# slow_task

# --network-configuration "awsvpcConfiguration={subnets=[subnet-0cc4dd3ae05f9c278,subnet-0779b66ce8c3a599c],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
