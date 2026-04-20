#!/bin/sh
while true; do
	wget -q -O - http://localhost:9090/metrics | grep '^ftp_sessions_total'
	sleep 1
done
