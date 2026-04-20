#!/usr/bin/env bash

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/zscaler_root.pem

TARGET=$1
if [[ -z "$TARGET" ]]; then
	echo "Usage: $0 <Service Name>"
	exit 1
fi

SPOT=$2
if [[ -z "$SPOT" ]]; then
	SPOT=FARGATE_SPOT
fi

for i in {1..1}; do
	aws ecs run-task \
		--count 1 \
		--cluster aeroftp-cluster \
		--capacity-provider-strategy capacityProvider=${SPOT},weight=1 \
		--network-configuration "awsvpcConfiguration={subnets=[subnet-0cc4dd3ae05f9c278,subnet-0779b66ce8c3a599c],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
		--task-definition aeroscrape:1 \
		--enable-execute-command \
		--overrides "{
			\"containerOverrides\": [{
				\"name\": \"aeroscrape\",
				\"environment\": [
					{\"name\": \"AEROSCRAPE_SERVICENAME\", \"value\": \"${TARGET}\"}
				]
			}]
		}"
done
