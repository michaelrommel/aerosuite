#!/usr/bin/env bash

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/zscaler_root.pem

SPOT=$1
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

aws ecs run-task \
	--no-cli-pager \
	--count 1 \
	--cluster aeroftp-cluster \
	--capacity-provider-strategy capacityProvider=${SPOT},weight=1 \
	--network-configuration "awsvpcConfiguration={subnets=[subnet-0779b66ce8c3a599c],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
	--task-definition aerocoach:5 \
	--enable-execute-command \
	--overrides "{
			\"containerOverrides\": [{
				\"name\": \"aerocoach\",
				\"environment\": [
					{\"name\": \"SPOT\", \"value\": \"${SPOT}\"}
				]
			}]
		}"
