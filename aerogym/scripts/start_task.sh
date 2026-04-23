#!/usr/bin/env bash

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/zscaler_root.pem

TARGET=$1
if [[ -z "$TARGET" ]]; then
	echo "Usage: $0 <AeroCoach container IP address>"
	exit 1
fi

SPOT=$2
if [[ -z "$SPOT" ]]; then
	SPOT=FARGATE_SPOT
fi

goto_gym() {
	ID=${1: -2}
	aws ecs run-task \
		--no-cli-pager \
		--count 1 \
		--cluster aeroftp-cluster \
		--capacity-provider-strategy capacityProvider=${SPOT},weight=1 \
		--network-configuration "awsvpcConfiguration={subnets=[subnet-0cc4dd3ae05f9c278],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
		--task-definition aerogym:5 \
		--enable-execute-command \
		--overrides "{
			\"containerOverrides\": [{
				\"name\": \"aerogym\",
				\"environment\": [
					{\"name\": \"AEROCOACH_URL\", \"value\": \"http://${TARGET}:50051\"},
					{\"name\": \"AEROGYM_AGENT_ID\", \"value\": \"a${ID}\"}
				]
			}]
		}"
}

for i in {1..1}; do
	goto_gym "0$i"
done

# --network-configuration "awsvpcConfiguration={subnets=[subnet-0cc4dd3ae05f9c278,subnet-0779b66ce8c3a599c],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
