#!/bin/sh
# Prepares what the Firecracker plugin boots, for its live test: the
# Firecracker binary ($FIRECRACKER_VERSION), the newest kernel from
# Firecracker's CI artifacts, and a root filesystem — Alpine's busybox has
# everything cuma-init and the test agent need — with /sbin/cuma-init.
#
# Usage: firecracker-assets.sh <dir>
# Prints, and appends to $GITHUB_ENV when set, CUMA_LIVE_FIRECRACKER_BIN,
# CUMA_LIVE_FIRECRACKER_KERNEL and CUMA_LIVE_FIRECRACKER_ROOTFS.
# Needs curl, docker, mkfs.ext4 and sudo (to keep root ownership in the image).
set -eu

dir=$1
repo=$(cd "$(dirname "$0")/../.." && pwd)
version=${FIRECRACKER_VERSION:?set FIRECRACKER_VERSION, e.g. v1.17.0}
rootfs_image=${ROOTFS_IMAGE:-public.ecr.aws/docker/library/alpine:3.22}
arch=$(uname -m)
mkdir -p "$dir"
cd "$dir"

curl -fsSL "https://github.com/firecracker-microvm/firecracker/releases/download/${version}/firecracker-${version}-${arch}.tgz" | tar -xz
mv "release-${version}-${arch}/firecracker-${version}-${arch}" firecracker
chmod +x firecracker
./firecracker --version | head -1

s3=https://s3.amazonaws.com/spec.ccfc.min
artifacts=$(curl -fsSL "$s3?list-type=2&prefix=firecracker-ci/&delimiter=/" \
  | grep -oP '(?<=<Prefix>)firecracker-ci/[0-9]{8}-[^/]+/(?=</Prefix>)' | sort | tail -1)
kernel=$(curl -fsSL "$s3?list-type=2&prefix=${artifacts}${arch}/vmlinux-" \
  | grep -oP "(?<=<Key>)(${artifacts}${arch}/vmlinux-[0-9]+\.[0-9]+\.[0-9]{1,3})(?=</Key>)" | sort -V | tail -1)
[ -n "$kernel" ] || { echo "no kernel found under ${artifacts}${arch}" >&2; exit 1; }
curl -fsSL -o vmlinux "$s3/$kernel"
echo "kernel: $kernel"

container=$(docker create "$rootfs_image")
mkdir -p rootfs
docker export "$container" | sudo tar -x -C rootfs
docker rm "$container" >/dev/null
sudo install -m 0755 "$repo/plugins/sandbox/firecracker/cuma-init" rootfs/sbin/cuma-init
truncate -s 256M rootfs.ext4
sudo mkfs.ext4 -q -F -d rootfs rootfs.ext4
sudo chown "$(id -u):$(id -g)" rootfs.ext4
sudo rm -rf rootfs

out="CUMA_LIVE_FIRECRACKER_BIN=$dir/firecracker
CUMA_LIVE_FIRECRACKER_KERNEL=$dir/vmlinux
CUMA_LIVE_FIRECRACKER_ROOTFS=$dir/rootfs.ext4"
echo "$out"
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "$out" >> "$GITHUB_ENV"
fi
