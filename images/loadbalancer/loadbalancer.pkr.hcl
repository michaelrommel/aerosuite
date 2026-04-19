#
# Packer file for creating the loadbalancer AMI image
#
# Run this with packer build backend.pkr.hcl from this directory.
# You need to have build the aeroscaler application in the development docker
# container with:
# cargo build --release --bin aeroscaler --target x86_64-unknown-linux-musl
#

source "amazon-ebs" "alpine" {
  ami_name      = "aeroscaler-alpine-{{timestamp}}"
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
    Name        = "aeroscaler-alpine"
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
      "sudo apk add --no-cache ca-certificates openssh nftables socat iputils binutils logrotate iproute2 conntrack-tools keepalived ipvsadm redis curl tcpdump",
      "sudo rc-update add sshd default",
      "sudo rc-update add nftables default",
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

  # Install all the helper applications for AWS management stuff
  provisioner "file" {
    source      = "../../../aeroscaler/target/release/aws-config"
    destination = "/tmp/aws-config"
  }
  provisioner "file" {
    source      = "../../../aeroscaler/target/release/assign-secondary-ip"
    destination = "/tmp/assign-secondary-ip"
  }
  provisioner "file" {
    source      = "../../../aeroscaler/target/release/keepalived-config"
    destination = "/tmp/keepalived-config"
  }
  provisioner "file" {
    source      = "../../../aeroscaler/target/release/attach-eni"
    destination = "/tmp/attach-eni"
  }
  provisioner "file" {
    source      = "../../../aeroscaler/target/release/slot-pool-native"
    destination = "/tmp/slot-pool-native"
  }
  provisioner "file" {
    source      = "../../../aeroscaler/target/release/aeroscaler"
    destination = "/tmp/aeroscaler"
  }

  # Install and configure the interface management
  provisioner "shell" {
    inline = [
      "sudo mkdir -p /usr/local/bin",
      "sudo mv /tmp/aws-config /usr/local/bin/aws-config",
      "sudo mv /tmp/assign-secondary-ip /usr/local/bin/assign-secondary-ip",
      "sudo mv /tmp/keepalived-config /usr/local/bin/keepalived-config",
      "sudo mv /tmp/attach-eni /usr/local/bin/attach-eni",
      "sudo mv /tmp/slot-pool-native /usr/local/bin/slot-pool-native",
      "sudo mv /tmp/aeroscaler /usr/local/bin/aeroscaler",
      "sudo chown root:root /usr/local/bin/aws-config",
      "sudo chown root:root /usr/local/bin/assign-secondary-ip",
      "sudo chown root:root /usr/local/bin/keepalived-config",
      "sudo chown root:root /usr/local/bin/attach-eni",
      "sudo chown root:root /usr/local/bin/slot-pool-native",
      "sudo chown root:root /usr/local/bin/aeroscaler",
      "sudo chmod +x /usr/local/bin/aws-config",
      "sudo chmod +x /usr/local/bin/assign-secondary-ip",
      "sudo chmod +x /usr/local/bin/keepalived-config",
      "sudo chmod +x /usr/local/bin/attach-eni",
      "sudo chmod +x /usr/local/bin/slot-pool-native",
      "sudo chmod +x /usr/local/bin/aeroscaler",
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

