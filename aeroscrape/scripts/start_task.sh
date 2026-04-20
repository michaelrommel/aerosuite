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
		--cluster ${CLUSTER} \
		--capacity-provider-strategy capacityProvider=${SPOT},weight=1 \
		--network-configuration "awsvpcConfiguration={subnets=[${SUBNET_PUBLIC1_REGION},${SUBNET_PUBLIC2_REGION}],securityGroups=[${SECURITY_GROUP_FTP}],assignPublicIp=ENABLED}" \
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
