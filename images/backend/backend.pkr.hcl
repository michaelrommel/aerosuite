#
# Packer file for creating the backend AMI image
#
# Run this with packer build backend.pkr.hcl from this directory.
# You need to have build the aeroftp application in the development docker
# container with:
# cargo build --release --bin aeroftp --target x86_64-unknown-linux-musl
#

source "amazon-ebs" "alpine" {
  ami_name      = "aeroftp-alpine-{{timestamp}}"
  # we need t3 because the images use uefi
  instance_type = "t3.micro"
  region        = "eu-west-2"
  vpc_id        = "vpc-0595e17ce290fb050"
  subnet_id     = "subnet-0c48fb2dcd6ce6c10"

  # Use an existing Alpine Linux AMI as the base
  # Find the latest: https://www.alpinelinux.org/cloud/
  source_ami_filter {
    filters = {
      name                = "alpine-3.*-x86_64-uefi-*"
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
  security_group_ids      = ["sg-06d737ea5595c275d"]

  # Assign a public IP so the instance is reachable through the bastion.
  associate_public_ip_address = true
  ssh_interface               = "public_ip"

  # Route SSH through the bastion; both legs authenticate via the ssh-agent.
  ssh_bastion_host       = "192.168.30.1"
  ssh_bastion_port       = 22
  ssh_bastion_username   = "rommel"
  ssh_bastion_agent_auth = true  # bastion leg: use agent key

  tags = {
    Name        = "aeroftp-alpine"
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
      "sudo apk add --no-cache ca-certificates openssh nftables curl",
      "sudo rc-update add sshd default",
      "sudo rc-update add nftables default",
    ]
  }

  # Install the CloudWatch agent
  provisioner "shell" {
    inline = [
      "curl -sSfO https://s3.amazonaws.com/amazoncloudwatch-agent/alpine/amd64/latest/amazon-cloudwatch-agent.apk",
      "sudo apk add --allow-untrusted amazon-cloudwatch-agent.apk",
      "rm amazon-cloudwatch-agent.apk",
    ]
  }

  # Install the CloudWatch agent configuration
  provisioner "file" {
    source      = "./_opt_aws_amazon-cloudwatch-agent_etc_amazon-cloudwatch-agent.json"
    destination = "/tmp/amazon-cloudwatch-agent.json"
  }

  provisioner "shell" {
    inline = [
      "sudo mkdir -p /opt/aws/amazon-cloudwatch-agent/etc",
      "sudo mv /tmp/amazon-cloudwatch-agent.json /opt/aws/amazon-cloudwatch-agent/etc/amazon-cloudwatch-agent.json",
      "sudo rc-update add amazon-cloudwatch-agent default",
    ]
  }

  # Inject the authorised public key for the alpine user
  provisioner "shell" {
    inline = [
      "mkdir -p /home/alpine/.ssh",
      "chmod 700 /home/alpine/.ssh",
      "echo 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKVSs3Pyvg/Y4e6p/5VkZU5LHsEqoT2EuZ/ZleZgTTkk rommel@crow' >> /home/alpine/.ssh/authorized_keys",
      "chmod 600 /home/alpine/.ssh/authorized_keys",
      "chown -R alpine:alpine /home/alpine/.ssh",
    ]
  }

  # Install nftables ruleset
  provisioner "file" {
    source      = "./_etc_nftables_aeroftp.nft"
    destination = "/tmp/_etc_nftables_aeroftp.nft"
  }

  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_etc_nftables_aeroftp.nft /etc/nftables.nft",
    ]
  }

  # Install the aeroftp-routing OpenRC service
  provisioner "file" {
    source      = "./_etc_init.d_aeroftp-routing"
    destination = "/tmp/_etc_init.d_aeroftp-routing"
  }

  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_etc_init.d_aeroftp-routing /etc/init.d/aeroftp-routing",
      "sudo chmod +x /etc/init.d/aeroftp-routing",
      "sudo rc-update add aeroftp-routing default",
    ]
  }
  # Create a dedicated aeroftp system user with a home directory
  provisioner "shell" {
    inline = [
      "sudo addgroup -S aeroftp",
      "sudo adduser -S -D -h /home/aeroftp -s /sbin/nologin -G aeroftp aeroftp",
      "sudo mkdir -p /home/aeroftp",
      "sudo chown aeroftp:aeroftp /home/aeroftp",
    ]
  }

  # Stage the binary via /tmp (writable by alpine), then move it into place
  provisioner "file" {
    source      = "../../target/release/aeroftp"
    destination = "/tmp/aeroftp"
  }

  # Install and configure the app
  provisioner "shell" {
    inline = [
      "sudo mv /tmp/aeroftp /home/aeroftp/aeroftp",
      "sudo chmod +x /home/aeroftp/aeroftp",
      "sudo chown aeroftp:aeroftp /home/aeroftp/aeroftp",
    ]
  }

  # Stage the credentials file via /tmp (writable by alpine), then move it into place
  provisioner "file" {
    source      = "./_home_aeroftp_credentials.json"
    destination = "/tmp/_home_aeroftp_credentials.json"
  }

  # Install and configure the app
  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_home_aeroftp_credentials.json /home/aeroftp/credentials.json",
      "sudo chown aeroftp:aeroftp /home/aeroftp/credentials.json",
    ]
  }

  # Write default service environment configuration
  provisioner "file" {
    source      = "./_etc_conf.d_aeroftp"
    destination = "/tmp/_etc_conf.d_aeroftp"
  }

  provisioner "shell" {
    inline = [
      "sudo mkdir -p /etc/conf.d",
      "sudo mv /tmp/_etc_conf.d_aeroftp /etc/conf.d/aeroftp",
    ]
  }

  # Install a systemd/openrc service file
  provisioner "file" {
    source      = "./_etc_init.d_aeroftp"
    destination = "/tmp/_etc_init.d_aeroftp"
  }

  provisioner "shell" {
    inline = [
      "sudo mv /tmp/_etc_init.d_aeroftp /etc/init.d/aeroftp",
      "sudo chmod +x /etc/init.d/aeroftp",
      "sudo rc-update add aeroftp default",
    ]
  }
}

