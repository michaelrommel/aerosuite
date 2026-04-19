
 Quick Start Guide

 ### 1. Install the scripts (run as root)

 ```bash
   cd /home/rommel/software/aerosuite/aeroftp
   sudo ./install-backend-manager.sh
 ```

 This will:
 - Copy manage-backend.sh to /usr/local/bin/
 - Install the systemd service
 - Start the weight watcher daemon automatically

 ### 2. Add backend slots to keepalived.conf

 Add these 20 real_server blocks (all DOWN by default):

 ```conf
   real_server 172.16.32.20 80 {
       weight 100
       file_check "/var/run/backend-172.16.32.20.healthy" {
           delay 2
       }
   }
   # ... repeat for IPs .21 through .39
 ```

 ### 3. Usage Examples

 ┌─────────────────────────────────────┬────────────────────────────────────────┐
 │ Command                             │ Effect                                 │
 ├─────────────────────────────────────┼────────────────────────────────────────┤
 │ sudo manage-backend.sh status       │ Show all backends (all DOWN initially) │
 ├─────────────────────────────────────┼────────────────────────────────────────┤
 │ sudo manage-backend.sh enable 25    │ Enable backend at full weight (100)    │
 ├─────────────────────────────────────┼────────────────────────────────────────┤
 │ sudo manage-backend.sh drain 25     │ Reduce to weight 0 (no new requests)   │
 ├─────────────────────────────────────┼────────────────────────────────────────┤
 │ sudo manage-backend.sh disable 25   │ Mark DOWN, remove from pool            │
 ├─────────────────────────────────────┼────────────────────────────────────────┤
 │ sudo manage-backend.sh weight 25 50 │ Set specific weight (50% traffic)      │
 └─────────────────────────────────────┴────────────────────────────────────────┘

 ────────────────────────────────────────────────────────────────────────────────

 Complete Lifecycle Example

 ```bash
   # Enable a backend when it's ready
   sudo manage-backend.sh enable 25

   # Gradually drain before maintenance
   sudo manage-backend.sh weight 25 50    # Reduce to 50%
   sleep 180                              # Wait for connections
   sudo manage-backend.sh weight 25 0     # Zero new requests
   sleep 60                               # Wait for drain complete

   # Mark DOWN when terminating
   sudo manage-backend.sh disable 25
 ```

 ────────────────────────────────────────────────────────────────────────────────

 Verify Installation

 ```bash
   # Check if watcher is running
   systemctl status backend-weight-watcher

   # View logs
   journalctl -u backend-weight-watcher -f

   # Test the manager
   manage-backend.sh help
 ```

