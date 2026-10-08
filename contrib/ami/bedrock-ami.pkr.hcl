# Bare-metal Bedrock host AMI: Debian 13 + Linux 6.18 with CONFIG_RUST=y.
#
# Built and verified on the hardware it targets: the build instance reboots
# into the new kernel, checks EPT-friendly PEBS, builds and loads bedrock.ko,
# and (by default) runs the bedrock-lab integration suite. Any failure fails
# the build, so no unverified AMI is produced. See README.md.

packer {
  required_plugins {
    amazon = {
      source  = "github.com/hashicorp/amazon"
      version = "~> 1.3"
    }
  }
}

variable "region" {
  type    = string
  default = "us-east-1"
}

variable "instance_type" {
  type        = string
  default     = "m7i.metal-24xl"
  description = "Must be bare metal with EPT-friendly PEBS (Ice Lake-SP or newer); verification fails otherwise."
}

variable "subnet_id" {
  type        = string
  default     = ""
  description = "Empty uses the default VPC."
}

variable "bedrock_src" {
  type        = string
  description = "git archive (tar.gz) of the bedrock repo used to verify the image: git archive --format=tar.gz -o bedrock-src.tar.gz HEAD"
}

variable "run_integration_tests" {
  type    = bool
  default = true
}

variable "ami_name_prefix" {
  type    = string
  default = "bedrock-debian13-linux6.18-rust"
}

locals {
  timestamp = formatdate("YYYYMMDD-hhmm", timestamp())
  tags = {
    Name          = "${var.ami_name_prefix}-${local.timestamp}"
    BedrockKernel = "6.18.0-bedrock"
    Rust          = "1.94.0"
    BaseImage     = "{{ .SourceAMIName }}"
  }
}

source "amazon-ebs" "bedrock" {
  region        = var.region
  instance_type = var.instance_type
  subnet_id     = var.subnet_id == "" ? null : var.subnet_id

  source_ami_filter {
    filters = {
      name                = "debian-13-amd64-*"
      architecture        = "x86_64"
      virtualization-type = "hvm"
      root-device-type    = "ebs"
    }
    # Debian cloud team: https://wiki.debian.org/Cloud/AmazonEC2Image
    owners      = ["136693071363"]
    most_recent = true
  }

  ssh_username = "admin"
  # Metal instances take several minutes to boot.
  ssh_timeout = "45m"

  ami_name     = "${var.ami_name_prefix}-${local.timestamp}"
  ena_support  = true
  imds_support = "v2.0"

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 2
  }

  # Kernel build tree (kept for out-of-tree module builds) plus Nix store.
  launch_block_device_mappings {
    device_name           = "/dev/xvda"
    volume_size           = 100
    volume_type           = "gp3"
    delete_on_termination = true
  }

  tags     = local.tags
  run_tags = local.tags
}

build {
  sources = ["source.amazon-ebs.bedrock"]

  provisioner "file" {
    source      = var.bedrock_src
    destination = "/tmp/bedrock-src.tar.gz"
  }

  provisioner "shell" {
    execute_command = "chmod +x {{ .Path }}; sudo -E env {{ .Vars }} bash {{ .Path }}"
    scripts = [
      "${path.root}/scripts/10-toolchain.sh",
      "${path.root}/scripts/20-kernel.sh",
    ]
  }

  # Boot the new kernel; it is GRUB's default (highest version).
  provisioner "shell" {
    execute_command   = "sudo -E bash {{ .Path }}"
    inline            = ["systemctl reboot"]
    expect_disconnect = true
  }

  provisioner "shell" {
    pause_before        = "60s"
    start_retry_timeout = "45m"
    execute_command     = "chmod +x {{ .Path }}; sudo -E env {{ .Vars }} bash {{ .Path }}"
    environment_vars    = ["RUN_INTEGRATION_TESTS=${var.run_integration_tests}"]
    scripts = [
      "${path.root}/scripts/30-verify.sh",
      "${path.root}/scripts/40-cleanup.sh",
    ]
  }

  post-processor "manifest" {
    output     = "${path.root}/manifest.json"
    strip_path = true
  }
}
