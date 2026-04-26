#!/usr/bin/env bash
# start_task.sh — launch the aeromon (Grafana / Tempo / Mimir / Prometheus) ECS task
# Usage: ./start_task.sh [FARGATE | FARGATE_SPOT]

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/zscaler_root.pem

CLUSTER=aeroftp-cluster
TASK_DEF=aeromon

CAPACITY=${1:-FARGATE}

aws ecs run-task \
	--no-cli-pager \
	--count 1 \
	--cluster "$CLUSTER" \
	--capacity-provider-strategy capacityProvider="${CAPACITY}",weight=1 \
	--network-configuration "awsvpcConfiguration={
		subnets=[subnet-0779b66ce8c3a599c],
		securityGroups=[sg-06d737ea5595c275d],
		assignPublicIp=ENABLED
	}" \
	--task-definition "$TASK_DEF" \
	--enable-execute-command

#subnets=[subnet-01b7cfc925f82cf7c],
