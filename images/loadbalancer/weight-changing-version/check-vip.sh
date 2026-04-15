#!/bin/bash
# Check if VIP is properly configured and listening

VIP="172.16.29.100"

if command -v ip &>/dev/null; then
	if ip addr show | grep -q "$VIP"; then
		exit 0
	fi
fi

exit 1
