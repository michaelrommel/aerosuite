#!/bin/bash
# Backend state checker for keepalived misc_check
# Interprets drain marker and returns appropriate weight adjustment

BACKEND_DIR="/var/run"

# Get backend index from argument (20-39)
backend_index="${1:-}"

if [ -z "$backend_index" ]; then
    echo "Usage: $0 <backend_index>" >&2
    exit 1
fi

ip="172.16.32.${backend_index}"
health_file="${BACKEND_DIR}/backend-${ip}.healthy"
drain_marker="${BACKEND_DIR}/backend-${ip}.draining"

# Check state and return:
# - Exit code determines UP/DOWN (0=UP, non-zero=DOWN)
# - Output value modifies weight if supported by keepalived version

if [ ! -f "$health_file" ]; then
    # No health file = DOWN (removed from pool)
    exit 1
fi

if [ -f "$drain_marker" ]; then
    # Drain marker present = UP but with reduced weight (0 for no new requests)
    # Output: return 0 for UP, and optionally output weight adjustment
    
    # For keepalived 2.3.x misc_check that supports weight output:
    echo "0"
    
    # Alternative approach - just exit 0 (UP) and rely on base weight being 100
    # The drain marker is read by external monitoring, not keepalived directly
    
    exit 0
fi

# Health file exists but no drain marker = UP with full weight
exit 0
