#!/usr/bin/env bash
# pod-up.sh — dev/validation helper: run the vx5g stack as one podman pod.
#
# All containers share the pod's network namespace, so the FiveGPlan's
# loopback addressing (127.0.0.x) works unchanged inside the pod — the same
# rendered configs run on host or in the pod, byte-identical.
#
# This is the reference for the vxedged fiveg lifecycle stage (which will own
# process supervision, subscriber provisioning via open5gs-dbctl, health and
# teardown); it is NOT the product path. Known gaps, tracked in TASKS.md §4:
# NRF/AUSF/UDM/UDR run stock configs from the image, and no subscribers are
# inserted yet — registration completes only after the lifecycle stage lands.
#
# Usage:
#   ./pod-up.sh <rendered-config-dir>     # dir produced by `render_fiveg`
#   ./pod-up.sh down
set -euo pipefail

POD=vx5g

if [[ "${1:-}" == "down" ]]; then
  podman pod rm -f "$POD" 2>/dev/null || true
  echo "vx5g pod removed"
  exit 0
fi

CFG="${1:?usage: pod-up.sh <rendered-config-dir>|down}"
[[ -d "$CFG/open5gs" ]] || { echo "error: $CFG/open5gs not found — render with: cargo run -p vxedge-core --example render_fiveg -- <manifest> $CFG" >&2; exit 1; }

podman pod exists "$POD" && { echo "error: pod $POD already exists — run '$0 down' first" >&2; exit 1; }
podman pod create --name "$POD" >/dev/null

run() { # run <name> <extra flags...> -- <image> <cmd...>
  local name="$1"; shift
  local flags=()
  while [[ "$1" != "--" ]]; do flags+=("$1"); shift; done
  shift
  podman run -d --pod "$POD" --name "$POD-$name" \
    -v "$(cd "$CFG" && pwd)":/etc/vx5g:ro,Z "${flags[@]}" "$@" >/dev/null
  echo "started $POD-$name"
}

# Data store (subscriber DB) + stock NRF from the image's default config
# (listens on 127.0.0.10 — matches the FiveGPlan's nrf_addr).
run mongo -- docker.io/library/mongo:7 mongod --bind_ip 127.0.0.1 --quiet
run nrf   -- vx5g-core open5gs-nrfd

# Rendered control plane.
run amf  -- vx5g-core open5gs-amfd  -c /etc/vx5g/open5gs/amf.yaml
run nssf -- vx5g-core open5gs-nssfd -c /etc/vx5g/open5gs/nssf.yaml
for f in "$CFG"/open5gs/smf-*.yaml; do
  s=$(basename "$f" .yaml); s=${s#smf-}
  run "smf-$s" -- vx5g-core open5gs-smfd -c "/etc/vx5g/open5gs/smf-$s.yaml"
done
# Per-slice UPFs create their vxtun<i> devices: tun + NET_ADMIN inside the pod.
for f in "$CFG"/open5gs/upf-*.yaml; do
  s=$(basename "$f" .yaml); s=${s#upf-}
  run "upf-$s" --cap-add=NET_ADMIN --device /dev/net/tun \
    -- vx5g-core open5gs-upfd -c "/etc/vx5g/open5gs/upf-$s.yaml"
done

echo "pod up — check: podman ps --pod --filter pod=$POD ; logs: podman logs $POD-amf"
echo "RAN (needs host 'modprobe sctp'):"
echo "  podman run --rm --pod $POD -v \$(pwd)/$CFG:/etc/vx5g:ro,Z vx5g-ran nr-gnb -c /etc/vx5g/ueransim/gnb.yaml"
