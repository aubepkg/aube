#!/bin/bash
set -euo pipefail

# Hosted runners are disposable. Mount a fresh image, never format a host disk.
[[ ${GITHUB_ACTIONS:-} == true && $(uname -s) == Linux ]]
volume=/mnt/aube-build
image="${RUNNER_TEMP:?}/aube-build.btrfs"
[[ ! -e $image && ! -e $volume ]]
if ! command -v mkfs.btrfs >/dev/null; then
	sudo apt-get update -qq
	sudo apt-get install -y --no-install-recommends btrfs-progs
fi
truncate -s 20G "$image"
mkfs.btrfs -q "$image"
sudo mkdir "$volume"
sudo mount -o loop,noatime "$image" "$volume"
sudo chown "$(id -u):$(id -g)" "$volume"
mkdir -m 700 "$volume/cache" "$volume/blaze" "$volume/cargo-target"
# Check the actual clone operation before spending time compiling.
printf 'clone probe\n' >"$volume/probe"
cp --reflink=always "$volume/probe" "$volume/probe-clone"
cmp "$volume/probe" "$volume/probe-clone"
{
	printf 'BLAZE_HOME=%s/blaze\n' "$volume"
	printf 'XDG_CACHE_HOME=%s/cache\n' "$volume"
	if [[ ${1:-} == --benchmark ]]; then
		printf 'CARGO_TARGET_DIR=%s/cargo-target\n' "$volume"
	fi
} >>"${GITHUB_ENV:?}"
findmnt --target "$volume"
