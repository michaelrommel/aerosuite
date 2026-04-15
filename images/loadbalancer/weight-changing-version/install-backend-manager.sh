#!/bin/bash
# Installation script for Keepalived Backend Manager
# Usage: sudo ./install-backend-manager.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANAGE_SCRIPT="${SCRIPT_DIR}/manage-backend.sh"
SERVICE_FILE="${SCRIPT_DIR}/backend-weight-watcher.service"

LOG_FILE="/var/log/backend-manager.log"
BACKEND_DIR="/var/run"

echo "=== Keepalived Backend Manager Installation ==="
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

# Copy manage-backend.sh to /usr/local/bin/
cp "$MANAGE_SCRIPT" /usr/local/bin/manage-backend.sh
chmod +x /usr/local/bin/manage-backend.sh
echo "Installed: /usr/local/bin/manage-backend.sh"

# Create backend state directory
mkdir -p "$BACKEND_DIR"
chmod 755 "$BACKEND_DIR"
echo "Created backend state directory: $BACKEND_DIR"

# Copy systemd service file to /etc/systemd/system/
cp "$SERVICE_FILE" /etc/systemd/system/backend-weight-watcher.service
echo "Installed service file: /etc/systemd/system/backend-weight-watcher.service"

# Reload systemd daemon
systemctl daemon-reload
echo "Reloaded systemd daemon"

# Enable and start the watcher service
systemctl enable backend-weight-watcher.service
systemctl restart backend-weight-watcher.service

if systemctl is-active --quiet backend-weight-watcher.service; then
    echo "Started: backend-weight-watcher.service (running)"
else
    echo "WARNING: Service failed to start. Check status with:"
    echo "  sudo systemctl status backend-weight-watcher"
fi

echo ""
echo "=== Installation Complete ==="
echo ""
echo "Next steps:"
echo "1. Add your keepalived.conf real_server blocks for IPs 172.16.32.20-39"
echo "   (All backends will be DOWN by default - no health files created)"
echo ""
echo "2. Usage examples:"
echo "   sudo manage-backend.sh enable <ip>      # Enable backend"
echo "   sudo manage-backend.sh drain <ip>       # Drain to zero weight"
echo "   sudo manage-backend.sh disable <ip>     # Mark DOWN"
echo "   sudo manage-backend.sh status           # Show all backends"
echo ""
echo "3. To view logs:"
echo "   journalctl -u backend-weight-watcher -f"
echo ""
