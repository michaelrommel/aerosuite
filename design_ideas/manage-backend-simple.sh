#!/bin/bash
# Simple Backend Manager for Keepalived 2.3.x
# Uses FILE_CHECK only (no misc_check scripts)
# Two states: ENABLED/DOWN + External draining via load balancer

set -euo pipefail

BACKEND_DIR="/var/run"
LOG_FILE="/var/log/backend-manager.log"

log() { 
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG_FILE"
}

# Validate IP format (20-39 range)
validate_ip() {
    local ip="$1"
    if [[ ! "$ip" =~ ^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$ ]]; then
        return 1
    fi
    
    local last_octet="${ip##*.}"
    [ "$last_octet" -ge 20 ] && [ "$last_octet" -le 39 ] || return 1
}

# Enable backend: Create health file (UP)
enable_backend() {
    local ip="$1"
    
    if ! validate_ip "$ip"; then
        log "ERROR: Invalid IP address or out of range: $ip" >&2
        exit 1
    fi
    
    rm -f "${BACKEND_DIR}/backend-${ip}.healthy" \
           "${BACKEND_DIR}/backend-${ip}.draining"
    
    touch "${BACKEND_DIR}/backend-${ip}.healthy"
    chmod 644 "${BACKEND_DIR}/backend-${ip}.healthy"
    
    log "ENABLED: 172.16.32.$ip - Receives full traffic (FILE_CHECK UP)"
}

# Drain backend: Create drain marker only (no health file change)
drain_backend() {
    local ip="$1"
    
    if ! validate_ip "$ip"; then
        log "ERROR: Invalid IP address or out of range: $ip" >&2
        exit 1
    fi
    
    # Keep the health file, just add drain marker for external monitoring
    touch "${BACKEND_DIR}/backend-${ip}.draining"
    chmod 644 "${BACKEND_DIR}/backend-${ip}.draining"
    
    log "DRAINING: 172.16.32.$ip - FILE_CHECK still UP, but mark for external draining"
}

# Disable backend: Remove all state files (DOWN)
disable_backend() {
    local ip="$1"
    
    if ! validate_ip "$ip"; then
        log "ERROR: Invalid IP address or out of range: $ip" >&2
        exit 1
    fi
    
    rm -f "${BACKEND_DIR}/backend-${ip}.healthy" \
           "${BACKEND_DIR}/backend-${ip}.draining"
    
    log "DISABLED: 172.16.32.$ip - FILE_CHECK DOWN, removed from pool"
}

# Show status of all backends or specific one
show_status() {
    local ip="${1:-}"
    
    if [ -n "$ip" ]; then
        echo "=== Backend 172.16.32.${ip} ==="
        
        local has_health=false
        local is_draining=false
        
        [ -f "${BACKEND_DIR}/backend-172.16.32.${ip}.healthy" ] && has_health=true
        [ -f "${BACKEND_DIR}/backend-172.16.32.${ip}.draining" ] && is_draining=true
        
        if [ "$is_draining" = true ]; then
            echo "Status: DRAINING (FILE_CHECK UP, marked for external draining)"
            echo "Keepalived sees: UP with weight 100"
            echo "External monitoring should stop sending traffic"
        elif [ "$has_health" = true ]; then
            echo "Status: ENABLED (FILE_CHECK UP)"
            echo "Keepalived sees: UP with weight 100, receives full traffic"
        else
            echo "Status: DISABLED (FILE_CHECK DOWN)"
            echo "Keepalived sees: DOWN, removed from pool"
        fi
        
    else
        # Show all backends status
        echo "=== All Backends Status (IP Range: 172.16.32.20-39) ==="
        echo ""
        
        local enabled=0
        local draining=0
        local disabled=0
        
        for i in {20..39}; do
            local has_health=false
            local is_draining=false
            
            [ -f "${BACKEND_DIR}/backend-172.16.32.${i}.healthy" ] && has_health=true
            [ -f "${BACKEND_DIR}/backend-172.16.32.${i}.draining" ] && is_draining=true
            
            if [ "$is_draining" = true ]; then
                printf "  [%s] 172.16.32.%-2d (FILE_CHECK UP, draining)\n" "DRAIN" "$i"
                ((draining++)) || true
            elif [ "$has_health" = true ]; then
                printf "  [%s] 172.16.32.%-2d (FILE_CHECK UP)\n" "ENABLED" "$i"
                ((enabled++)) || true
            else
                printf "  [%s] 172.16.32.%-2d (FILE_CHECK DOWN)\n" "DISABLED" "$i"
                ((disabled++)) || true
            fi
        done
        
        echo ""
        printf "Summary: %d ENABLED | %d DRAINING | %d DISABLED\n" "$enabled" "$draining" "$disabled"
        echo ""
        echo "Note: Keepalived sees backends as UP/DOWN only."
        echo "External load balancer (haproxy/nginx/etc.) should handle actual draining."
    fi
}

# Generate keepalived config using FILE_CHECK only
generate_config() {
    cat <<EOF
# Keepalived Configuration - FILE_CHECK Only (2 States)
# No misc_check scripts needed - faster and simpler!

virtual_server 192.168.1.100 80 {
    delay_loop 3
    lb_algo rr
    lb_kind NAT
    
EOF

    for i in {20..39}; do
        cat <<EOF
    real_server 172.16.32.$i 80 {
        weight 100
        
        # FILE_CHECK only - simple and efficient
        file_check "/var/run/backend-172.16.32.${i}.healthy" {
            delay 2
        }
    }

EOF
    done
    
    echo "}"
}

# Generate external draining script example
generate_drainer() {
    cat <<'EOF'
#!/bin/bash
# External Draining Monitor - Runs every few seconds
# Adjusts load balancer config based on drain markers

BACKEND_DIR="/var/run"
LB_CONFIG="/etc/haproxy/haproxy.cfg"  # Example: HAProxy config

# This script should be run by your external load balancer's health check
# or a separate monitoring daemon that adjusts actual traffic routing

for f in "${BACKEND_DIR}"/backend-*.draining; do
    [ -f "$f" ] || continue
    
    ip=$(basename "$f" .draining | sed 's/backend-172\.16\.32\./172.16.32./')
    
    echo "Backend $ip is draining - adjust load balancer to stop sending traffic"
    
    # Example: For HAProxy, update server weight via socat/cookie
    # echo "disable server <pool>/<ip>" | socat stdio /var/run/haproxy.sock
    
done

# Or for nginx upstream (reload config if needed)
EOF
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
    
    status)
        show_status "${2:-}"
        ;;
    
    generate-config)
        generate_config
        ;;
    
    help|--help|-h|"") 
        cat <<EOF
Backend Manager for Keepalived 2.3.x - FILE_CHECK Only

Usage: $0 <command> [args]

IP Range: 172.16.32.20-39 (20 slots)

States:
  ENABLED   = HEALTH file only → Keepalived sees UP (weight 100)
  DRAINING  = HEALTH + DRAIN file → Keepalived still sees UP, 
              but external load balancer should stop traffic
  DISABLED  = No files → Keepalived sees DOWN

Key Difference from misc_check approach:
  - FILE_CHECK is native to keepalived (no script overhead)
  - Keepalived only knows UP/DOWN (not draining state)
  - External load balancer must handle actual traffic draining

Commands:
  enable <ip>         Set backend to ENABLED (FILE_CHECK UP)
  drain <ip>          Mark for DRAINING (external monitoring needed)
  disable <ip>        Set backend to DISABLED (FILE_CHECK DOWN)
  status [ip]         Show all backends or specific one
  generate-config     Generate keepalived.conf with FILE_CHECK

Examples - Complete Lifecycle:
  # When ready to accept traffic:
  \$0 enable 25
  
  # Before maintenance, mark for draining:
  \$0 drain 25
  
  # External load balancer sees .draining file and stops sending traffic
  # (You need to implement this monitoring)
  
  sleep 300  # Wait for connections to finish
  
  # When terminating completely:
  \$0 disable 25

Advantages over misc_check:
  ✓ No script execution overhead per backend
  ✓ Keepalived native FILE_CHECK (faster)
  ✓ Simpler configuration
  ✓ Works with any keepalived version

Disadvantage:
  ✗ External load balancer must implement draining logic
EOF
        ;;
    
    *)
        echo "Unknown command: $1" >&2
        exit 1
        ;;
esac
