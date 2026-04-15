#!/bin/bash
# Simplified Backend Manager for Keepalived 2.3.x
# Three clear states: ENABLED, DRAINING, DISABLED
# IP Range: 172.16.32.20-39 (20 slots)

set -euo pipefail

BACKEND_DIR="/var/run"
KEEPALIVED_CONF="/etc/keepalived/keepalived.conf"
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
    
    # Extract last octet and check range
    local last_octet="${ip##*.}"
    if [ "$last_octet" -lt 20 ] || [ "$last_octet" -gt 39 ]; then
        return 1
    fi
    
    return 0
}

# Enable backend: Create health file only (no drain marker)
enable_backend() {
    local ip="$1"
    
    if ! validate_ip "$ip"; then
        log "ERROR: Invalid IP address or out of range: $ip" >&2
        exit 1
    fi
    
    # Remove any existing state files first
    rm -f "${BACKEND_DIR}/backend-${ip}.healthy" \
           "${BACKEND_DIR}/backend-${ip}.draining"
    
    # Create health file (UP with weight 100)
    touch "${BACKEND_DIR}/backend-${ip}.healthy"
    chmod 644 "${BACKEND_DIR}/backend-${ip}.healthy"
    
    log "ENABLED: 172.16.32.$ip - Receives full traffic (weight 100)"
}

# Drain backend: Create drain marker + health file
drain_backend() {
    local ip="$1"
    
    if ! validate_ip "$ip"; then
        log "ERROR: Invalid IP address or out of range: $ip" >&2
        exit 1
    fi
    
    # Remove any existing state files first
    rm -f "${BACKEND_DIR}/backend-${ip}.healthy" \
           "${BACKEND_DIR}/backend-${ip}.draining"
    
    # Create drain marker (UP with weight 0)
    touch "${BACKEND_DIR}/backend-${ip}.draining"
    chmod 644 "${BACKEND_DIR}/backend-${ip}.draining"
    
    log "DRAINING: 172.16.32.$ip - No new requests, existing connections continue"
}

# Disable backend: Remove all state files (DOWN)
disable_backend() {
    local ip="$1"
    
    if ! validate_ip "$ip"; then
        log "ERROR: Invalid IP address or out of range: $ip" >&2
        exit 1
    fi
    
    # Remove all state files
    rm -f "${BACKEND_DIR}/backend-${ip}.healthy" \
           "${BACKEND_DIR}/backend-${ip}.draining"
    
    log "DISABLED: 172.16.32.$ip - Removed from pool completely"
}

# Show status of all backends or specific one
show_status() {
    local ip="${1:-}"
    
    if [ -n "$ip" ]; then
        echo "=== Backend 172.16.32.${ip} ==="
        
        # Check state files
        local has_health=false
        local is_draining=false
        
        [ -f "${BACKEND_DIR}/backend-172.16.32.${ip}.healthy" ] && has_health=true
        [ -f "${BACKEND_DIR}/backend-172.16.32.${ip}.draining" ] && is_draining=true
        
        if [ "$is_draining" = true ]; then
            echo "Status: DRAINING (no new requests)"
            echo "Health file exists, drain marker present - weight 0"
        elif [ "$has_health" = true ]; then
            echo "Status: ENABLED (receiving traffic)"
            echo "Health file exists - weight 100"
        else
            echo "Status: DISABLED (not in pool)"
            echo "No state files present"
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
                printf "  [%s] 172.16.32.%-2d\n" "DRAIN" "$i"
                ((draining++)) || true
            elif [ "$has_health" = true ]; then
                printf "  [%s] 172.16.32.%-2d\n" "ENABLED" "$i"
                ((enabled++)) || true
            else
                printf "  [%s] 172.16.32.%-2d\n" "DISABLED" "$i"
                ((disabled++)) || true
            fi
        done
        
        echo ""
        printf "Summary: %d ENABLED | %d DRAINING | %d DISABLED\n" "$enabled" "$draining" "$disabled"
    fi
}

# Generate configuration snippet for keepalived.conf
generate_config() {
    cat <<EOF
# Add these to your /etc/keepalived/keepalived.conf real_server blocks:
# All backends start DISABLED (no health files created initially)

virtual_server 192.168.1.100 80 {
    delay_loop 3
    lb_algo rr
    lb_kind NAT
    
EOF

    for i in {20..39}; do
        cat <<EOF
    real_server 172.16.32.$i 80 {
        weight 100
        
        # State management:
        # - ENABLED: Create backend-172.16.32.${i}.healthy (no .draining file)
        # - DRAINING: Create backend-172.16.32.${i}.draining + health file
        # - DISABLED: Remove both files
        
        misc_check "/usr/local/bin/backend-state-check.sh ${i}" {
            delay 2
        }
    }

EOF
    done
    
    echo "}"
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
Backend Manager for Keepalived 2.3.x - Three Clear States

Usage: $0 <command> [args]

IP Range: 172.16.32.20-39 (20 slots)

States:
  ENABLED   = Receives full traffic (weight 100, health file only)
  DRAINING  = No new requests, existing connections continue 
              (draining marker + health file, weight 0)
  DISABLED  = Removed from pool completely (no files)

Commands:
  enable <ip>         Set backend to ENABLED state
                      Example: \$0 enable 25
  
  drain <ip>          Set backend to DRAINING state
                      Example: \$0 drain 25
  
  disable <ip>        Set backend to DISABLED state
                      Example: \$0 disable 25
  
  status [ip]         Show all backends or specific one
                      Example: \$0 status          # All backends
                      Example: \$0 status 25       # Single IP
  
  generate-config     Generate keepalived.conf snippet with misc_check

Examples - Complete Lifecycle:
  # Start: Backend is DISABLED (no files)
  
  # When ready to accept traffic:
  \$0 enable 25        # ENABLED - receives full traffic
  
  # Before maintenance/shutdown, drain gracefully:
  \$0 drain 25         # DRAINING - no new requests
  
  # Wait for existing connections to finish (e.g., 5 minutes)
  sleep 300
  
  # When terminating completely:
  \$0 disable 25       # DISABLED - removed from pool

Note: 
- All backends start DISABLED by default (no state files)
- State files are in /var/run/ with prefix 'backend-'
- Requires misc_check script to interpret drain marker
EOF
        ;;
    
    *)
        echo "Unknown command: $1" >&2
        exit 1
        ;;
esac
