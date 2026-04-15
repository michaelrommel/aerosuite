#!/bin/bash
# Complete backend manager for keepalived 2.3.x
# Usage: manage-backend.sh <command> [args]
# IP Range: 172.16.32.20-39 (20 slots)

set -euo pipefail

BACKEND_DIR="/var/run"
KEEPALIVED_CONF="/etc/keepalived/keepalived.conf"
LOG_FILE="/var/log/backend-manager.log"

log() {
	echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG_FILE"
}

# Enable backend (create health file)
enable_backend() {
	local ip="$1"

	if ! validate_ip "$ip"; then
		log "ERROR: Invalid IP address: $ip" >&2
		exit 1
	fi

	touch "${BACKEND_DIR}/backend-${ip}.healthy"
	chmod 644 "${BACKEND_DIR}/backend-${ip}.healthy"

	# Reload keepalived to apply immediately
	systemctl reload keepalived || log "WARNING: Could not reload keepalived (may need sudo)"

	log "ENABLED: 172.16.32.$ip (weight: 100, receives traffic)"
}

# Drain backend (reduce weight to 0 via config reload)
drain_backend() {
	local ip="$1"

	if ! validate_ip "$ip"; then
		log "ERROR: Invalid IP address: $ip" >&2
		exit 1
	fi

	# Set target weight marker for the watcher daemon
	echo "0" >"${BACKEND_DIR}/backend-${ip}.target-weight"

	log "DRAINING: 172.16.32.$ip (weight -> 0, no new requests)"
}

# Disable backend (remove health file)
disable_backend() {
	local ip="$1"

	if ! validate_ip "$ip"; then
		log "ERROR: Invalid IP address: $ip" >&2
		exit 1
	fi

	rm -f "${BACKEND_DIR}/backend-${ip}.healthy" \
		"${BACKEND_DIR}/backend-${ip}.target-weight" \
		"${BACKEND_DIR}/backend-${ip}.draining"

	# Reload keepalived to apply immediately
	systemctl reload keepalived || log "WARNING: Could not reload keepalived (may need sudo)"

	log "DISABLED: 172.16.32.$ip (marked DOWN, removed from pool)"
}

# Set specific weight (triggers config reload via watcher)
set_weight() {
	local ip="$1"
	local weight="$2"

	if ! validate_ip "$ip"; then
		log "ERROR: Invalid IP address: $ip" >&2
		exit 1
	fi

	# Validate weight range (0-256)
	if [ "$weight" -lt 0 ] || [ "$weight" -gt 256 ]; then
		log "ERROR: Weight must be between 0 and 256, got $weight" >&2
		exit 1
	fi

	# Create marker for weight change watcher daemon
	echo "$weight" >"${BACKEND_DIR}/backend-${ip}.target-weight"

	log "SET WEIGHT: 172.16.32.$ip (weight -> $weight)"
}

# Show status of all backends or specific one
show_status() {
	local ip="${1:-}"

	if [ -n "$ip" ]; then
		echo "=== Backend 172.16.32.${ip} ==="

		if [ -f "${BACKEND_DIR}/backend-172.16.32.${ip}.healthy" ]; then
			echo "Status: UP (receives traffic)"

			if [ -f "${BACKEND_DIR}/backend-172.16.32.${ip}.target-weight" ]; then
				local w=$(cat "${BACKEND_DIR}/backend-172.16.32.${ip}.target-weight")
				echo "Pending weight change: $w (waiting for watcher daemon)"
			else
				echo "Current weight: 100 (base weight from keepalived.conf)"
			fi
		else
			echo "Status: DOWN (not in load balancing pool)"

			if [ -f "${BACKEND_DIR}/backend-172.16.32.${ip}.target-weight" ]; then
				local w=$(cat "${BACKEND_DIR}/backend-172.16.32.${ip}.target-weight")
				echo "Pending weight change: $w (will be applied when re-enabled)"
			fi
		fi

		if [ -f "${BACKEND_DIR}/backend-172.16.32.${ip}.draining" ]; then
			local w=$(cat "${BACKEND_DIR}/backend-172.16.32.${ip}.draining")
			echo "Drain weight: $w"
		fi

	else
		# Show all backends status
		echo "=== All Backends Status (IP Range: 172.16.32.20-39) ==="
		echo ""

		local up_count=0
		local down_count=0

		for i in {20..39}; do
			local status="DOWN"

			if [ -f "${BACKEND_DIR}/backend-172.16.32.${i}.healthy" ]; then
				status="UP"
				((up_count++)) || true
			else
				((down_count++)) || true
			fi

			printf "  [%s] 172.16.32.%-2d\n" "$status" "$i"
		done

		echo ""
		printf "Summary: %d UP, %d DOWN\n" "$up_count" "$down_count"

		# Show pending weight changes
		local has_pending=false
		echo ""
		for f in "${BACKEND_DIR}"/backend-*.target-weight; do
			[ -f "$f" ] || continue

			local ip=$(basename "$f" .target-weight | sed 's/backend-172\.16\.32\./172.16.32./')
			echo "  [PENDING WEIGHT] $ip -> $(cat "$f") (waiting for watcher daemon)"
			has_pending=true
		done

		if [ "$has_pending" = false ]; then
			echo ""
			echo "No pending weight changes."
		fi
	fi
}

# Validate IP format
validate_ip() {
	[[ "$1" =~ ^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$ ]]
}

# Watcher daemon - applies weight changes periodically
watch_daemon() {
	log "Starting weight change watcher (PID: $$)"

	while true; do
		for f in "${BACKEND_DIR}"/backend-*.target-weight; do
			[ -f "$f" ] || continue

			local ip=$(basename "$f" .target-weight | sed 's/backend-172\.16\.32\./172.16.32./')
			local weight=$(cat "$f")

			log "Applying weight change: $ip -> $weight"

			# Update keepalived config for this specific IP using perl
			perl -i -pe 's/(real_server 172\.16\.32\.\Q'."$ip\E"' 21 \{[^}]*weight)\s+\d+/$1 '"$weight"'/e' "$KEEPALIVED_CONF" 2>/dev/null || {
				log "WARNING: Could not update config via perl, trying sed fallback" >&2

				# Fallback to sed (less reliable for multi-line)
				sed -i "/real_server ${ip} 21/,/^    }/{s/weight [0-9]*/weight ${weight}/}" "$KEEPALIVED_CONF"
			}

			rm -f "$f"

			# Reload keepalived to apply weight change
			if keepalived -t >/dev/null 2>&1 && systemctl reload keepalived; then
				log "Applied weight $weight to $ip successfully"
			else
				log "ERROR: Failed to validate or reload keepalived config for $ip" >&2
			fi
		done

		sleep 2
	done
}

# Generate configuration snippet for new backend slots
generate_config() {
	local start_ip="${1:-20}"
	local end_ip="${2:-39}"

	echo "# Add these to your keepalived.conf real_server blocks:"
	echo ""
	for i in $(seq "$start_ip" "$end_ip"); do
		cat <<EOF

    real_server 172.16.32.$i 21 {
        weight 100
        file_check "/var/run/backend-172.16.32.$i.healthy" {
            delay 2
        }
    }
EOF
	done
}

# Main command handler
case "${1:-help}" in
enable)
	[ -z "$2" ] && echo "Usage: $0 enable <ip>" >&2 && exit 1
	enable_backend "$2"
	;;

drain)
	[ -z "$2" ] && echo "Usage: $0 drain <ip>" >&2 && exit 1
	drain_backend "$2"
	;;

disable)
	[ -z "$2" ] && echo "Usage: $0 disable <ip>" >&2 && exit 1
	disable_backend "$2"
	;;

weight | --weight)
	[ -z "${3:-}" ] && echo "Usage: $0 weight <ip> <weight>" >&2 && exit 1
	set_weight "$2" "$3"
	;;

status)
	show_status "${2:-}"
	;;

watch)
	watch_daemon
	;;

generate-config)
	generate_config "${2:-20}" "${3:-39}"
	;;

help | --help | -h | "")
	cat <<EOF
Backend Manager for Keepalived 2.3.x

Usage: $0 <command> [args]

IP Range: 172.16.32.20-39 (20 slots)

Commands:
  enable <ip>         Enable backend at full weight (receives traffic)
                      Example: \$0 enable 25
  
  drain <ip>          Reduce weight to 0 (drain mode, no new requests)
                      Example: \$0 drain 25
  
  disable <ip>        Mark backend DOWN (remove from pool)
                      Example: \$0 disable 25
  
  weight <ip> <w>     Set specific weight (0-256)
                      Example: \$0 weight 25 50
  
  status [ip]         Show all backends or specific one status
                      Example: \$0 status          # All backends
                      Example: \$0 status 25       # Single backend
  
  watch               Start the weight change watcher daemon (run as service)
  
  generate-config     Generate config snippets for new backend slots

Examples - Complete Lifecycle:
  # Enable a backend
  \$0 enable 25
  
  # Gradually drain it (reduce traffic before shutdown)
  \$0 weight 25 50    # Reduce to 50% traffic
  sleep 180           # Wait for connections to finish
  \$0 weight 25 0     # Zero new requests
  
  # Mark DOWN when terminating
  \$0 disable 25

Note: 
- Weight changes require keepalived reload (handled automatically by watcher daemon)
- Run 'watch' command as a background service for automatic weight application
- Health files are created in /var/run/ with prefix 'backend-'
EOF
	;;

*)
	echo "Unknown command: $1" >&2
	exit 1
	;;
esac
