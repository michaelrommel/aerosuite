# 1. Clear any existing qdisc rules
sudo tc qdisc del dev eth0 root

# 2. Add root handle defaulting unclassified traffic to class 20
sudo tc qdisc add dev eth0 root handle 1: htb default 20

# 3. Define total baseline bandwidth (970mbit)
sudo tc class add dev eth0 parent 1: classid 1:1 htb rate 1970mbit

# 4. Create SSH High-Priority Class (Priority 1)
# Gives SSH at least 20 Mbps, bursting up to the full 1970 Mbps
sudo tc class add dev eth0 parent 1:1 classid 1:10 htb rate 20mbit ceil 1970mbit prio 1

# 5. Create Default Class for everything else (Priority 2)
# Gives other traffic the remaining 1950 Mbps, bursting up to the full 1970 Mbps
sudo tc class add dev eth0 parent 1:1 classid 1:20 htb rate 1950mbit ceil 1970mbit prio 2

# 6. Apply filters to isolate SSH traffic (Port 22)
sudo tc filter add dev eth0 protocol ip parent 1:0 prio 1 u32 match ip sport 22 0xffff flowid 1:10
sudo tc filter add dev eth0 protocol ip parent 1:0 prio 1 u32 match ip dport 22 0xffff flowid 1:10

