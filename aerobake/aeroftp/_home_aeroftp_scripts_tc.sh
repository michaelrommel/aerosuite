#!/bin/sh
set -e

# Bandwidth in mbit/s (should be the AWS baseline - not burst)
BANDWIDTH=5000
# Guard against AWS Traffic shaping on Hypervisor level
CEILING=$((BANDWIDTH - 20))
# How much to reserve for SSH traffic in percent
SSH=5
# the remainder of the traffic
STUFF=$((100 - SSH))
# list all interfaces
INTERFACES="eth0 eth1"

NUM_INTS=0
for _i in $INTERFACES; do
	NUM_INTS=$((NUM_INTS + 1))
done

if [ "$NUM_INTS" -eq 0 ]; then
	echo "Error: No interfaces specified in \$INTERFACES."
	exit 1
fi

# Divide total bandwidth equally across all specified interfaces
# Using native integer division (rounding down)
INT_BANDWIDTH=$((CEILING / NUM_INTS))

# Calculate specific class bandwidths for this interface
SSH_RATE=$(((INT_BANDWIDTH * SSH) / 100))
DEFAULT_RATE=$(((INT_BANDWIDTH * STUFF) / 100))

# Enforce minimum allocation floor of 1mbit to keep classes valid
[ "$SSH_RATE" -lt 1 ] && SSH_RATE=1
[ "$DEFAULT_RATE" -lt 1 ] && DEFAULT_RATE=1

for int in ${INTERFACES}; do
	# 1. Clear any existing qdisc rules
	sudo tc qdisc del dev ${int} root 2>/dev/null || true

	if [ "$1" = "set" ]; then
		# 2. Add root handle defaulting unclassified traffic to class 20
		sudo tc qdisc add dev ${int} root handle 1: htb default 20

		# 3. Define total baseline bandwidth (970mbit)
		sudo tc class add dev ${int} parent 1: classid 1:1 htb rate ${INT_BANDWIDTH}mbit

		# 4. Create SSH High-Priority Class (Priority 1)
		# Gives SSH at least 20 Mbps, bursting up to the full 1970 Mbps
		sudo tc class add dev ${int} parent 1:1 classid 1:10 htb rate ${SSH_RATE}mbit ceil ${INT_BANDWIDTH}mbit prio 1

		# 5. Create Default Class for everything else (Priority 2)
		# Gives other traffic the remaining 1950 Mbps, bursting up to the full 1970 Mbps
		sudo tc class add dev ${int} parent 1:1 classid 1:20 htb rate ${DEFAULT_RATE}mbit ceil ${INT_BANDWIDTH}mbit prio 2

		# 6. Apply filters to isolate SSH traffic (Port 22)
		sudo tc filter add dev ${int} protocol ip parent 1:0 prio 1 u32 match ip sport 22 0xffff flowid 1:10
		sudo tc filter add dev ${int} protocol ip parent 1:0 prio 1 u32 match ip dport 22 0xffff flowid 1:10
	fi
done
