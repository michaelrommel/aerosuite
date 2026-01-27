#!/usr/bin/env bash

export REQUESTS_CA_BUNDLE=/etc/ssl/certs/zscaler_root.pem

CREDENTIALS=$(aws-sso-util credential-process --profile logsan_stats_complete_484651457934 --sso-start-url https://shsconsole.awsapps.com/start --sso-region us-east-1)
if [[ $? != 0 ]]; then
	CHECK=$(aws-sso-util check --sso-start-url https://shsconsole.awsapps.com/start --sso-region us-east-1 --force-refresh)
	if [[ $? != 0 ]]; then
		echo "No credentials and refresh failed"
		exit 1
	else
		CREDENTIALS=$(aws-sso-util credential-process --profile logsan_stats_complete_484651457934 --sso-start-url https://shsconsole.awsapps.com/start --sso-region us-east-1)
		if [[ $? != 0 ]]; then
			echo "login successful but no credentials found"
			exit 1
		fi
	fi
fi

# Credentials should now be there:
# {"Version":1,"AccessKeyId":"ASIXXXXXXXXXXXXXXOMI","SecretAccessKey":"bSGfpXXXXXXXXXXXXXXXXXXXXXXXXXXXsYPhH980","SessionToken":"IQoJbXXXXXXjOSBROZ","Expiration":"2026-01-12T21:30:29Z"}

AWS_ACCESS_KEY_ID=$(echo "$CREDENTIALS" | jq -r ".AccessKeyId")
AWS_SECRET_ACCESS_KEY=$(echo "$CREDENTIALS" | jq -r ".SecretAccessKey")
AWS_SESSION_TOKEN=$(echo "$CREDENTIALS" | jq -r ".SessionToken")
export AWS_ACCESS_KEY_ID
export AWS_SECRET_ACCESS_KEY
export AWS_SESSION_TOKEN

# Call given program
"$@"
