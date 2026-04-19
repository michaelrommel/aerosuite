#!/bin/bash
# External Draining Monitor for Keepalived + FILE_CHECK approach
# Monitors drain markers and adjusts load balancer configuration

set -euo pipefail

BACKEND_DIR="/var/run"
LOG_FILE="/var/log/backend-drain-monitor.log"
LB_STATE_DIR="/var/lib/lb-state"  # Directory to store LB state changes

mkdir -p "$LB_STATE_DIR"

log() { 
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG_FILE"
}

# This function should be customized for your load balancer:
# Options: haproxy, nginx, traefik, or custom HTTP backend
adjust_load_balancer() {
    local ip="$1"
    local action="$2"  # "stop_traffic" | "start_traffic"
    
    log "Adjusting LB traffic for $ip: $action"
    
    case "$action" in
        stop_traffic)
            # Example for HAProxy (uncomment and customize):
            # echo "disable server pool_name/${ip##*.}" | socat stdio /var/run/haproxy.sock
            
            # Example for nginx (reload config with modified upstream):
            # sed -i "s/server ${ip} max_fails=100;/server ${ip} down;/" /etc/nginx/upstream.conf
            # nginx -s reload
            
            # For now, just write to state file for external processing
            echo "stop" > "${LB_STATE_DIR}/lb-${ip##*.}"
            
            ;;
        start_traffic)
            # Example for HAProxy:
            # echo "enable server pool_name/${ip##*.}" | socat stdio /var/run/haproxy.sock
            
            # For nginx:
            # sed -i "s/server ${ip} down;/server ${ip} max_fails=10;/" /etc/nginx/upstream.conf
            # nginx -s reload
            
            echo "start" > "${LB_STATE_DIR}/lb-${ip##*.}"
            
            ;;
    esac
}

# Main monitoring loop
monitor_drains() {
    log "Starting drain monitor (PID: $$)"
    
    while true; do
        local changes=false
        
        # Check for backends that started draining
        for f in "${BACKEND_DIR}"/backend-*.draining; do
            [ -f "$f" ] || continue
            
            local ip=$(basename "$f" .draining | sed 's/backend-172\.16\.32\./172.16.32./')
            
            # Check if already stopped in LB state
            if [ ! -f "${LB_STATE_DIR}/lb-${ip##*.}" ]; then
                adjust_load_balancer "$ip" "stop_traffic"
                log "Stopped traffic to $ip (draining detected)"
                changes=true
            fi
        done
        
        # Check for backends that stopped draining (health file exists, no drain marker)
        for f in "${BACKEND_DIR}"/backend-*.healthy; do
            [ -f "$f" ] || continue
            
            local ip=$(basename "$f" .healthy | sed 's/backend-172\.16\.32\./172.16.32./')
            
            # If no drain marker, resume traffic
            if [ ! -f "${BACKEND_DIR}/backend-${ip}.draining" ]; then
                adjust_load_balancer "$ip" "start_traffic"
                log "Resumed traffic to $ip (no longer draining)"
                changes=true
            fi
        done
        
        # Check for backends that were disabled completely
        for f in "${LB_STATE_DIR}"/lb-*; do
            [ -f "$f" ] || continue
            
            local idx=$(basename "$f" | sed 's/lb-//')
            local ip="172.16.32.${idx}"
            
            # If both health and drain files are gone, LB state is stale
            if [ ! -f "${BACKEND_DIR}/backend-${ip}.healthy" ] && \
               [ ! -f "${BACKEND_DIR}/backend-${ip}.draining" ]; then
                rm -f "$f"
                log "Cleaned up stale LB state for $ip"
                changes=true
            fi
        done
        
        # Sleep between checks (adjust frequency as needed)
        sleep 5
        
    done
}

# One-time check (for use in scripts or cron)
check_once() {
    local changed=0
    
    for f in "${BACKEND_DIR}"/backend-*.draining; do
        [ -f "$f" ] || continue
        
        local ip=$(basename "$f" .draining | sed 's/backend-172\.16\.32\./172.16.32./')
        
        if [ ! -f "${LB_STATE_DIR}/lb-${ip##*.}" ]; then
            adjust_load_balancer "$ip" "stop_traffic"
            log "One-time: Stopped traffic to $ip (draining detected)"
            ((changed++)) || true
        fi
    done
    
    echo "Processed $changed drain markers in this run"
}

# Main command handler
case "${1:-monitor}" in
    monitor)
        monitor_drains
        ;;
    
    check-once|once)
        check_once
        ;;
    
    help|--help|-h|"") 
        cat <<EOF
External Draining Monitor for FILE_CHECK approach

Usage: $0 <command>

Commands:
  monitor       Run continuously in foreground (recommended as daemon)
  once          Check drain markers and adjust LB once, then exit
  
How it works:
  - Monitors /var/run/backend-*.draining files
  - When .draining appears: stops traffic to that backend in your load balancer
  - When .draining disappears: resumes traffic (if health file exists)
  
You need to customize the adjust_load_balancer() function for your LB:
  - HAProxy: Use socat with haproxy.sock
  - Nginx: Reload config after modifying upstream blocks
  - Custom: Send API calls or update your own state files

Examples:
  # Run as systemd service (recommended)
  \$0 monitor
  
  # Or run via cron every minute
  */1 * * * * /usr/local/bin/drain-monitor.sh once

EOF
        ;;
    
    *)
        echo "Unknown command: $1" >&2
        exit 1
        ;;
esac
