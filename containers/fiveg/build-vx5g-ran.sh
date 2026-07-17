#!/usr/bin/env bash
# Builds the vx5g-ran image. vxprobe (vxagents, a separate git repo not
# nested under this one) can't be reached directly from this build context
# -- Containerfiles are sandboxed to their own context -- so this stages the
# prebuilt binary here first. Standard practice for a cross-repo binary
# dependency without a shared monorepo context, not a workaround.
set -euo pipefail
cd "$(dirname "$0")"

VXAGENTS_DIR="../../../vxagents"
if [ ! -d "$VXAGENTS_DIR" ]; then
  echo "error: expected sibling repo at $VXAGENTS_DIR (vxedge/vxagents), not found" >&2
  exit 1
fi

cargo build --release -p vxprobe --manifest-path "$VXAGENTS_DIR/Cargo.toml"
cp "$VXAGENTS_DIR/target/release/vxprobe" ./vxprobe.bin
trap 'rm -f ./vxprobe.bin' EXIT

podman build -t vx5g-ran -f Containerfile.ueransim .
