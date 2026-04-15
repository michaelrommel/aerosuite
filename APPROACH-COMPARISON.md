# Keepalived Backend Manager - Approach Comparison

## The Core Question

**How to implement 3 states (ENABLED, DRAINING, DISABLED) in keepalived 2.3.x?**

---

## Option A: FILE_CHECK Only (Recommended for 2.3.x)

### Configuration
```conf
real_server 172.16.32.25 80 {
    weight 100
    
    # Native keepalived check - no script overhead!
    file_check "/var/run/backend-172.16.32.25.healthy" {
        delay 2
    }
}
```

### State Management
| State | Marker Files | Keepalived Sees | Traffic Flow |
|-------|-------------|-----------------|--------------|
| **ENABLED** | `.healthy` only | UP (weight 100) | ✅ Full traffic |
| **DRAINING** | `.healthy` + `.draining` | Still sees UP (weight 100) | ⚠️ Must stop externally |
| **DISABLED** | No files | DOWN | ❌ Removed from pool |

### Pros
- ✅ **Fast**: FILE_CHECK is native, just stat() on file
- ✅ **No script overhead**: 20 backends × every check = no extra processes
- ✅ **Simple**: Keepalived handles everything natively
- ✅ **Works everywhere**: Any keepalived version supports it

### Cons
- ⚠️ **External draining required**: Your load balancer (HAProxy/nginx/etc.) must read `.draining` files and stop sending traffic
- ⚠️ **Keepalived doesn't know about DRAINING**: It still sees backends as UP with full weight

### Implementation
```bash
# Enable backend
sudo manage-backend-simple.sh enable 25

# Mark for draining (external monitor reads .draining file)
sudo manage-backend-simple.sh drain 25

# Disable completely
sudo manage-backend-simple.sh disable 25

# Start external drainer daemon
sudo drain-monitor.sh &
```

---

## Option B: misc_check Scripts (What I showed earlier)

### Configuration
```conf
real_server 172.16.32.25 80 {
    weight 100
    
    # Custom script runs every check interval
    misc_check "/usr/local/bin/backend-state-check.sh 25" {
        delay 2
    }
}
```

### State Management
| State | Marker Files | Keepalived Sees | Traffic Flow |
|-------|-------------|-----------------|--------------|
| **ENABLED** | `.healthy` only | UP (weight 100) | ✅ Full traffic |
| **DRAINING** | `.healthy` + `.draining` | UP (can output weight=0) | ✅ Stop via keepalived |
| **DISABLED** | No files | DOWN | ❌ Removed from pool |

### Pros
- ✅ **Full control**: Keepalived itself handles draining via weight adjustment
- ✅ **No external dependency**: Everything in one system
- ✅ **Clean API**: `manage-backend.sh` controls everything

### Cons
- ⚠️ **Script overhead**: 20 backends × check interval = 20 script executions per check
- ⚠️ **Slower**: Shell scripts take more time than FILE_CHECK stat()
- ⚠️ **More complex**: Requires custom script maintenance

### Example Overhead
```
Keepalived check interval: every 3 seconds
Backends: 20
Script execution time: ~1ms per backend (optimistic)

Total overhead: 20 scripts × 1ms = 20ms every 3 seconds
≈ 6.7 checks/second of CPU time just for health checks
```

---

## Recommendation

### For Keepalived 2.3.x → **Use FILE_CHECK Only (Option A)**

**Why?**
1. Your version doesn't have the dynamic weight API anyway
2. The script overhead is unnecessary complexity
3. External draining via load balancer is actually a better architecture:
   - Keepalived handles health checks efficiently
   - Load balancer handles traffic routing intelligently (connection draining, graceful shutdown)

### For Keepalived 2.5+ → **Use misc_check with Dynamic Weight**

If you upgrade to keepalived 2.5+, the dynamic weight API makes Option B viable again.

---

## Hybrid Approach (Best of Both Worlds?)

```
Keepalived FILE_CHECK: Fast health detection (UP/DOWN)
External Load Balancer: Intelligent traffic management (draining, connection limits)
Drain Monitor Script: Bridges the two by reading .draining files
```

This separates concerns:
- **Keepalived**: "Is this backend healthy?" → Fast file check
- **Load Balancer**: "How should I route traffic?" → Can read drain markers via API or config reload
- **Drain Monitor**: "Translate state files to LB actions" → Lightweight script

---

## Decision Matrix

| Your Priority | Recommended Approach |
|--------------|---------------------|
| Performance / Simplicity | FILE_CHECK + External Draining (Option A) |
| Keepalived-only solution | misc_check scripts (Option B) |
| Production with HAProxy/nginx | FILE_CHECK + LB integration (Option A) |
| Need graceful shutdown in one system | misc_check + weight adjustment (Option B) |

---

## Files for Each Option

### Option A: FILE_CHECK Only
- `manage-backend-simple.sh` - CLI tool
- `drain-monitor.sh` - External draining daemon
- Uses native `file_check` directive

### Option B: misc_check Scripts
- `manage-backend.sh` (original) - CLI tool with weight support
- `backend-state-check.sh` - Keepalived health script
- Uses custom `misc_check` directive

---

## Bottom Line

**For your keepalived 2.3.x setup, I recommend Option A (FILE_CHECK only).** 

The overhead of running 40 scripts per minute (20 backends × every 3 seconds) isn't worth it when you can achieve the same result with a simple file check and external load balancer integration.

Would you like me to:
1. Show how to integrate with HAProxy specifically?
2. Create a systemd service for the drain monitor?
3. Provide both options so you can test?
