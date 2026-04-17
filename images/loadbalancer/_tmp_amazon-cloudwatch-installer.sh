#!/bin/sh

mkdir /tmp/cwa
cd /tmp/cwa || exit
curl -sSfO https://s3.amazonaws.com/amazoncloudwatch-agent/debian/amd64/latest/amazon-cloudwatch-agent.deb
ar x amazon-cloudwatch-agent.deb
tar zxf data.tar.gz

if ! grep "^cwagent:" /etc/group; then
	addgroup -S cwagent
	echo "create group cwagent, result: $?"
fi
if ! id cwagent >/dev/null 2>&1; then
	adduser -S -D -h /home/cwagent -G cwagent -g "Cloudwatch Agent" -s $(test -x /sbin/nologin && echo /sbin/nologin || (test -x /usr/sbin/nologin && echo /usr/sbin/nologin || (test -x /bin/false && echo /bin/false || echo /bin/sh))) cwagent
	echo "create user cwagent, result: $?"
fi

mkdir -p /opt/aws/
mkdir -p /etc/amazon/
mkdir -p /var/log/amazon/
mkdir -p /var/run/amazon/

mv ./opt/aws/amazon-cloudwatch-agent /opt/aws/
ln -sf /opt/aws/amazon-cloudwatch-agent/etc /etc/amazon/amazon-cloudwatch-agent
ln -sf /opt/aws/amazon-cloudwatch-agent/bin/amazon-cloudwatch-agent-ctl /usr/bin/amazon-cloudwatch-agent-ctl
ln -sf /opt/aws/amazon-cloudwatch-agent/logs /var/log/amazon/amazon-cloudwatch-agent
ln -sf /opt/aws/amazon-cloudwatch-agent/var /var/run/amazon/amazon-cloudwatch-agent

cd /tmp || exit 1
rm -rf cwa
rm -f /tmp/amazon-cloudwatch-installer.sh
