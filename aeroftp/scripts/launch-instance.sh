#!/bin/bash
set -e

AMI=$1

if [[ -z "$AMI" ]]; then
	echo "Usage: $0 <AMI Image>"
	exit 1
fi

INSTANCE_ID=$(aws ec2 run-instances \
	--image-id "${AMI}" \
	--instance-type t3.micro \
	--region eu-west-2 \
	--key-name rommel@md151vfc \
	--subnet-id subnet-0c48fb2dcd6ce6c10 \
	--security-group-ids sg-06d737ea5595c275d \
	--associate-public-ip-address \
	--iam-instance-profile Name="ecsInstanceRole" \
	--tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=aeroftp-backend-test1}]' \
	--query 'Instances[0].InstanceId' \
	--output text)

echo "Launched instance: $INSTANCE_ID"
echo "Waiting for instance to reach running state..."

aws ec2 wait instance-running --instance-ids "$INSTANCE_ID"

PUBLIC_IP=$(aws ec2 describe-instances \
	--instance-ids "$INSTANCE_ID" \
	--query 'Reservations[0].Instances[0].PublicIpAddress' \
	--output text)

echo "Instance is running."
echo "  ID:        $INSTANCE_ID"
echo "  Public IP: $PUBLIC_IP"
