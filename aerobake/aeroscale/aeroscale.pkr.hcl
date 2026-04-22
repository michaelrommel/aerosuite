#
# Packer file for creating the loadbalancer AMI image
#
# Run this with packer build loadbalancer.pkr.hcl from this directory.
# You need to have built the aeroscale binaries in the development docker
# container with:
# cargo build --release --target x86_64-unknown-linux-musl
#

source "amazon-ebs" "alpine" {
  ami_name      = "aeroscale-alpine-{{timestamp}}"
  # we need t3 because the images use uefi
  instance_type = "t3.micro"
  region        = "${REGION}"
  vpc_id        = "${VPC_ID}"
  subnet_id     = "${SUBNET_BACKEND_PUBLIC}"

  # Use an existing Alpine Linux AMI as the base
  # Find the latest: https://www.alpinelinux.org/cloud/
  source_ami_filter {
    filters = {
      # fix a version exactly, as time sorting is flaky and might
      # give you an older version, 3.21 instead of 3.23
      name                = "alpine-3.23.3-x86_64-uefi-tiny-r0"
      root-device-type    = "ebs"
      virtualization-type = "hvm"
    }
    owners      = ["538276064493"] # Alpine Linux official AWS account
    most_recent = true
  }

  # Install sudo as root via cloud-init before any provisioner runs.
  # The alpine user is already in the wheel group; this grants it
  # passwordless sudo access for all subsequent provisioning steps.
  user_data = <<-EOF
    #!/bin/sh
    apk add --no-cache sudo
    echo '%wheel ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/wheel
  EOF

  ssh_username = "alpine"

  # Wait for the instance to fully boot before the first SSH attempt,
  # reducing the number of rapid retries that can trigger rate limits.
  pause_before_connecting = "90s"
  # Total window in which Packer keeps retrying SSH (default: 5m).
  ssh_timeout             = "5m"

  # Use a pre-existing security group instead of Packer's temporary one.
  # The SG must allow inbound SSH (port 22) from your build host / bastion,
  # NOT from 0.0.0.0/0, to satisfy your policy.
  security_group_ids      = ["${SECURITY_GROUP_FTP}"]

  # Assign a public IP so the instance is reachable through the bastion.
  associate_public_ip_address = true
  ssh_interface               = "public_ip"

  # Route SSH through the bastion; both legs authenticate via the ssh-agent.
  ssh_bastion_host       = "192.168.30.1"
  ssh_bastion_port       = 22
  ssh_bastion_username   = "rommel"
  ssh_bastion_agent_auth = true  # bastion leg: use agent key

  tags = {
    Name        = "aeroscale-alpine"
    Environment = "production"
    BuildDate   = "{{timestamp}}"
  }
}

build {
  sources = ["source.amazon-ebs.alpine"]

  # Install dependencies
  provisioner "shell" {
    inline = [
      "sudo apk update",
      "sudo apk add --no-cache ca-certificates openssh nftables socat iputils binutils logrotate iproute2 conntrack-tools keepalived ipvsadm procps redis curl tcpdump dnsmasq",
      "sudo rc-update add sshd default",
      "sudo rc-update add nftables default",
      "sudo rc-update add dnsmasq default",
    ]
  }

  # root aliases, I hate this
  provisioner "file" {
    source      = "./_root_.profile"
    destination = "/tmp/_root_.profile"
  }
  provisioner "file" {
    source      = "./_root_.ashrc"
    destination = "/tmp/_root_.ashrc"
  }
  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_root_.profile /root/.profile",
      "sudo mv /tmp/_root_.ashrc /root/.ashrc",
      "sudo chown root:root /root/.profile",
      "sudo chown root:root /root/.ashrc",
    ]
  }

  # Install the CloudWatch agent
  provisioner "file" {
    source      = "./_tmp_amazon-cloudwatch-installer.sh"
    destination = "/tmp/amazon-cloudwatch-installer.sh"
  }
  provisioner "shell" {
    inline = [
      "sudo /bin/sh /tmp/amazon-cloudwatch-installer.sh"
    ]
  }

  # Install the CloudWatch agent configuration
  provisioner "file" {
    source      = "./_opt_aws_amazon-cloudwatch-agent_etc_amazon-cloudwatch-agent.json"
    destination = "/tmp/amazon-cloudwatch-agent.json"
  }
  provisioner "file" {
    source      = "./_opt_aws_amazon-cloudwatch-agent_etc_amazon-cloudwatch-agent.toml"
    destination = "/tmp/amazon-cloudwatch-agent.toml"
  }
  # Install the cloudwatch-agent OpenRC service
  provisioner "file" {
    source      = "./_etc_init.d_amazon-cloudwatch-agent"
    destination = "/tmp/_etc_init.d_amazon-cloudwatch-agent"
  }

  provisioner "shell" {
    inline = [
      "sudo mkdir -p /opt/aws/amazon-cloudwatch-agent/etc",
      "sudo mv /tmp/amazon-cloudwatch-agent.json /opt/aws/amazon-cloudwatch-agent/etc/amazon-cloudwatch-agent.json",
      "sudo mv /tmp/amazon-cloudwatch-agent.toml /opt/aws/amazon-cloudwatch-agent/etc/amazon-cloudwatch-agent.toml",
      "sudo mv /tmp/_etc_init.d_amazon-cloudwatch-agent /etc/init.d/amazon-cloudwatch-agent",
      "sudo chmod +x /etc/init.d/amazon-cloudwatch-agent",
      "sudo rc-update add amazon-cloudwatch-agent default",
    ]
  }

  # redirect logging of keepalived by using the local3 facility
  provisioner "file" {
    source      = "./_etc_syslog.conf"
    destination = "/tmp/_etc_syslog.conf"
  }

  provisioner "shell" {
    inline = [
      "sudo mkdir -p /var/log/keepalived",
      "sudo mv /tmp/_etc_syslog.conf /etc/syslog.conf",
      "sudo chown root:root /etc/syslog.conf",
    ]
  }

  # Enable log rotation
  provisioner "file" {
    source      = "./_etc_logrotate.d_keepalived"
    destination = "/tmp/_etc_logrotate.d_keepalived"
  }

  provisioner "shell" {
    inline = [
      "sudo mkdir -p /etc/logrotate.d",
      "sudo mv /tmp/_etc_logrotate.d_keepalived /etc/logrotate.d/keepalived",
      "sudo chown root:root /etc/logrotate.d/keepalived",
    ]
  }

  # Install dnsmasq — local caching resolver to survive transient VPC DNS
  # outages that would otherwise cause the ASG query to fail and trigger
  # spurious backend cleanup cycles.
  provisioner "file" {
    source      = "./_etc_dnsmasq.d_aeroscale.conf"
    destination = "/tmp/_etc_dnsmasq.d_aeroscale.conf"
  }
  provisioner "file" {
    source      = "./_etc_dhcpcd.conf"
    destination = "/tmp/_etc_dhcpcd.conf"
  }
  provisioner "file" {
    source      = "./_etc_resolv.conf"
    destination = "/tmp/_etc_resolv.conf"
  }

  provisioner "shell" {
    inline = [
      "sudo mkdir -p /etc/dnsmasq.d",
      "sudo mv /tmp/_etc_dnsmasq.d_aeroscale.conf /etc/dnsmasq.d/aeroscale.conf",
      "sudo chown root:root /etc/dnsmasq.d/aeroscale.conf",
      # Replace dhcpcd.conf — nohook resolv.conf prevents DHCP renewals
      # from overwriting the static resolv.conf that points to dnsmasq.
      "sudo mv /tmp/_etc_dhcpcd.conf /etc/dhcpcd.conf",
      "sudo chown root:root /etc/dhcpcd.conf",
      # Install static resolv.conf last so it wins over any dhcpcd hook
      # that may have already run during provisioning.
      "sudo mv /tmp/_etc_resolv.conf /etc/resolv.conf",
      "sudo chown root:root /etc/resolv.conf",
    ]
  }

  # Install sysctl tweaks
  provisioner "file" {
    source      = "./_etc_sysctl.d_50-aeroscaler.conf"
    destination = "/tmp/_etc_sysctl.d_50-aeroscaler.conf"
  }

  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_etc_sysctl.d_50-aeroscaler.conf /etc/sysctl.d/50-aeroscaler.conf",
      "sudo chown root:root /etc/sysctl.d/50-aeroscaler.conf",
    ]
  }

  # Modules to load
  provisioner "file" {
    source      = "./_etc_modules-load.d_keepalived.conf"
    destination = "/tmp/_etc_modules-load.d_keepalived.conf"
  }

  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_etc_modules-load.d_keepalived.conf /etc/modules-load.d/keepalived.conf",
      "sudo chown root:root /etc/modules-load.d/keepalived.conf",
    ]
  }

  # Install nftables ruleset
  provisioner "file" {
    source      = "./_etc_nftables_aeroscaler.nft"
    destination = "/tmp/_etc_nftables_aeroscaler.nft"
  }

  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_etc_nftables_aeroscaler.nft /etc/nftables.nft",
      "sudo chown root:root /etc/nftables.nft",
    ]
  }

  provisioner "file" {
    source      = "../../target/release/aws-config"
    destination = "/tmp/aws-config"
  }
  provisioner "file" {
    source      = "../../target/release/scale"
    destination = "/tmp/scale"
  }
  provisioner "file" {
    source      = "../../target/release/aeroplug"
    destination = "/tmp/aeroplug"
  }
  provisioner "file" {
    source      = "../../target/release/aeropulse"
    destination = "/tmp/aeropulse"
  }
  provisioner "file" {
    source      = "../../target/release/aeroslot"
    destination = "/tmp/aeroslot"
  }
  provisioner "file" {
    source      = "../../target/release/aeroscale"
    destination = "/tmp/aeroscale"
  }

  # Install and configure the interface management
  provisioner "shell" {
    inline = [
      "sudo mkdir -p /usr/local/bin",
      "sudo mv /tmp/aws-config /usr/local/bin/aws-config",
      "sudo mv /tmp/scale /usr/local/bin/scale",
      "sudo mv /tmp/aeroplug /usr/local/bin/aeroplug",
      "sudo mv /tmp/aeropulse /usr/local/bin/aeropulse",
      "sudo mv /tmp/aeroslot /usr/local/bin/aeroslot",
      "sudo mv /tmp/aeroscale /usr/local/bin/aeroscale",
      "sudo chown root:root /usr/local/bin/aws-config",
      "sudo chown root:root /usr/local/bin/scale",
      "sudo chown root:root /usr/local/bin/aeroplug",
      "sudo chown root:root /usr/local/bin/aeropulse",
      "sudo chown root:root /usr/local/bin/aeroslot",
      "sudo chown root:root /usr/local/bin/aeroscale",
      "sudo chmod +x /usr/local/bin/aws-config",
      "sudo chmod +x /usr/local/bin/scale",
      "sudo chmod +x /usr/local/bin/aeroplug",
      "sudo chmod +x /usr/local/bin/aeropulse",
      "sudo chmod +x /usr/local/bin/aeroslot",
      "sudo chmod +x /usr/local/bin/aeroscale",
    ]
  }

  # Configure the loadbalancer
  provisioner "file" {
    source      = "./_etc_init.d_keepalived"
    destination = "/tmp/_etc_init.d_keepalived"
  }
  provisioner "file" {
    source      = "./_etc_conf.d_keepalived"
    destination = "/tmp/_etc_conf.d_keepalived"
  }
  provisioner "file" {
    source      = "./_etc_keepalived_keepalived.conf"
    destination = "/tmp/_etc_keepalived_keepalived.conf"
  }

  provisioner "shell" {
    inline = [
      "sudo mkdir -p /etc/keepalived",
      "sudo adduser -S -D -H -s /sbin/nologin keepalived_script",
      "sudo mv /tmp/_etc_conf.d_keepalived /etc/conf.d/keepalived",
      "sudo mv /tmp/_etc_keepalived_keepalived.conf /etc/keepalived/keepalived.conf",
      "sudo mv /tmp/_etc_init.d_keepalived /etc/init.d/keepalived",
      "sudo chown root:root /etc/conf.d/keepalived",
      "sudo chown root:root /etc/keepalived/keepalived.conf",
      "sudo chown root:root /etc/init.d/keepalived",
      "sudo chmod +x /etc/init.d/keepalived",
      "sudo rc-update add keepalived default",
    ]
  }

  # Install the aeroscale OpenRC service
  provisioner "file" {
    source      = "./_etc_init.d_aeroscale"
    destination = "/tmp/_etc_init.d_aeroscale"
  }
  provisioner "file" {
    source      = "./_etc_conf.d_aeroscale"
    destination = "/tmp/_etc_conf.d_aeroscale"
  }
  provisioner "file" {
    source      = "./_etc_logrotate.d_aeroscale"
    destination = "/tmp/_etc_logrotate.d_aeroscale"
  }

  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_etc_conf.d_aeroscale /etc/conf.d/aeroscale",
      "sudo mv /tmp/_etc_init.d_aeroscale /etc/init.d/aeroscale",
      "sudo chmod +x /etc/init.d/aeroscale",
      "sudo mv /tmp/_etc_logrotate.d_aeroscale /etc/logrotate.d/aeroscale",
      "sudo chown root:root /etc/conf.d/aeroscale",
      "sudo chown root:root /etc/init.d/aeroscale",
      "sudo chown root:root /etc/logrotate.d/aeroscale",
      "sudo rc-update add aeroscale default",
    ]
  }

  # return image to a state where tiny-cloud runs again to provision
  # ssh keys etc.
  provisioner "shell" {
    inline = [
      "sudo rm -rf /var/lib/cloud",
      "sudo rm -f /etc/hostname",
      "sudo tiny-cloud --bootstrap incomplete",
      "sudo truncate -s 0 /etc/machine-id",
      "sudo truncate -s 0 /var/log/*.log",
      "history -c"
    ]
  }
}

