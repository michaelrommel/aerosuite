#!/bin/bash
# Installation script for Simplified Keepalived Backend Manager
# Usage: sudo ./install-backend-manager.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANAGE_SCRIPT="${SCRIPT_DIR}/manage-backend.sh"
STATE_CHECK="${SCRIPT_DIR}/backend-state-check.sh"
LOG_FILE="/var/log/backend-manager.log"
BACKEND_DIR="/var/run"

echo "=== Simplified Backend Manager Installation ==="
echo ""

# Check for root privileges
if [ "$EUID" -ne 0 ]; then
    echo "ERROR: This script must be run as root (use sudo)" >&2
    exit 1
fi

# Create log directory and file
mkdir -p "$(dirname "$LOG_FILE")"
touch "$LOG_FILE"
chmod 644 "$LOG_FILE"
echo "Created log file: $LOG_FILE"

# Copy scripts to /usr/local/bin/
cp "$MANAGE_SCRIPT" /usr/local/bin/manage-backend.sh
chmod +x /usr/local/bin/manage-backend.sh
echo "Installed: /usr/local/bin/manage-backend.sh"

cp "$STATE_CHECK" /usr/local/bin/backend-state-check.sh
chmod +x /usr/local/bin/backend-state-check.sh
echo "Installed: /usr/local/bin/backend-state-check.sh"

# Create backend state directory
mkdir -p "$BACKEND_DIR"
chmod 755 "$BACKEND_DIR"
echo "Created backend state directory: $BACKEND_DIR"

echo ""
echo "=== Installation Complete ==="
echo ""
echo "Three clear states:"
echo "  ENABLED   = Receives full traffic (weight 100)"
echo "  DRAINING  = No new requests, existing continue"
echo "  DISABLED  = Removed from pool completely"
echo ""
echo "Usage examples:"
echo "  sudo manage-backend.sh enable <ip>      # Enable backend"
echo "  sudo manage-backend.sh drain <ip>       # Drain (no new requests)"
echo "  sudo manage-backend.sh disable <ip>     # Mark DOWN"
echo "  sudo manage-backend.sh status           # Show all backends"
echo ""
echo "Keepalived configuration:"
echo "Add misc_check to each real_server block pointing to:"
echo "  /usr/local/bin/backend-state-check.sh <backend_index>"
echo ""
