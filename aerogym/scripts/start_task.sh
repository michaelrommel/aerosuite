#!/usr/bin/env bash

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/zscaler_root.pem

# TARGET=$1
# if [[ -z "$TARGET" ]]; then
# 	echo "Usage: $0 <AeroCoach container IP address>"
# 	exit 1
# fi
CLUSTER=aeroftp-cluster

SPOT=$1
if [[ -z "$SPOT" ]]; then
	SPOT=FARGATE_SPOT
fi

# Get the current running task's public IP for the agents
TASK_ARN=$(aws ecs list-tasks \
	--cluster $CLUSTER --service-name aerotrack \
	--query "taskArns[0]" --output text)

COACH_IP=$(aws ecs describe-tasks \
	--cluster $CLUSTER --tasks $TASK_ARN \
	--query "tasks[0].attachments[0].details[?name=='privateIPv4Address'].value" \
	--output text)

# COACH_IP=$(aws ec2 describe-network-interfaces \
# 	--network-interface-ids $ENI_ID \
# 	--query "NetworkInterfaces[0].Association.PublicIp" --output text)

echo "aerocoach reachable at $COACH_IP:50051"

goto_gym() {
	ID=${1: -2}
	aws ecs run-task \
		--no-cli-pager \
		--count 1 \
		--cluster $CLUSTER \
		--capacity-provider-strategy capacityProvider=${SPOT},weight=1 \
		--network-configuration "awsvpcConfiguration={subnets=[subnet-0cc4dd3ae05f9c278],securityGroups=[sg-06d737ea5595c275d],assignPublicIp=ENABLED}" \
		--task-definition aerogym \
		--enable-execute-command \
		--overrides "{
			\"containerOverrides\": [{
				\"name\": \"aerogym\",
				\"environment\": [
					{\"name\": \"AEROCOACH_URL\", \"value\": \"http://${COACH_IP}:50051\"},
					{\"name\": \"AEROGYM_AGENT_ID\", \"value\": \"a${ID}\"}
				]
			}]
		}"
}

# Launch agents pointing at aerocoach's public gRPC port
for i in {1..3}; do
	goto_gym "0$i"
done
