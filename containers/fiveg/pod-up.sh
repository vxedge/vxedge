#!/usr/bin/env bash
# pod-up.sh — dev/validation helper: run the vx5g stack as one podman pod.
#
# All containers share the pod's network namespace, so the FiveGPlan's
# loopback addressing (127.0.0.x) works unchanged inside the pod — the same
# rendered configs run on host or in the pod, byte-identical.
#
# This is the reference for the vxedged fiveg lifecycle stage (which will own
# process supervision, health, and teardown); it is NOT the product path.
# Subscriber insertion uses open5gs-dbctl (baked into vx5g-core) with the
# credentials parsed from the rendered ue-*.yaml files — the rendered configs
# stay the single source of truth. Known gap, tracked in TASKS.md §4: dbctl's
# add_ue_with_slice requires an SD, so slices declared without one register
# via plain `add` (default APN) — the Rust lifecycle stage will write the
# subscriber document explicitly instead.
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
CFG="$(cd "$CFG" && pwd)"

podman pod exists "$POD" && { echo "error: pod $POD already exists — run '$0 down' first" >&2; exit 1; }
podman pod create --name "$POD" >/dev/null

run() { # run <name> <extra flags...> -- <image> <cmd...>
  local name="$1"; shift
  local flags=()
  while [[ "$1" != "--" ]]; do flags+=("$1"); shift; done
  shift
  podman run -d --pod "$POD" --name "$POD-$name" \
    -v "$CFG":/etc/vx5g:ro,Z "${flags[@]}" "$@" >/dev/null
  echo "started $POD-$name"
}

nf() { run "$1" -- vx5g-core "open5gs-$1d" -c "/etc/vx5g/open5gs/$1.yaml"; }

# Dependency order: store -> NRF -> the SBI plane -> AMF -> per-slice SMF/UPF.
run mongo -- docker.io/library/mongo:7 mongod --bind_ip 127.0.0.1 --quiet
sleep 2
nf nrf
sleep 1
for n in ausf udm udr pcf bsf nssf; do nf "$n"; done
sleep 1
nf amf
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

# Subscribers: one per rendered UE, credentials parsed from the ue yaml.
sleep 2
for f in "$CFG"/ueransim/ue-*.yaml; do
  imsi=$(grep "^supi:" "$f" | grep -o '[0-9]\{15\}')
  key=$(grep "^key:" "$f" | grep -o "[0-9A-Fa-f]\{32\}")
  opc=$(grep "^op:" "$f" | grep -o "[0-9A-Fa-f]\{32\}")
  apn=$(grep "apn:" "$f" | sed "s/.*apn: '\(.*\)'/\1/")
  sst=$(grep -A2 "slice:" "$f" | grep "sst:" | head -1 | grep -o '[0-9]\+')
  sd=$(grep -A3 "slice:" "$f" | grep "sd:" | head -1 | grep -o '[0-9A-Fa-f]\{6\}' || true)
  if [[ -n "$sd" ]]; then
    podman run --rm --pod "$POD" vx5g-core \
      open5gs-dbctl add_ue_with_slice "$imsi" "$key" "$opc" "$apn" "$sst" "$sd" >/dev/null
    echo "subscriber $imsi (apn=$apn sst=$sst sd=$sd)"
  else
    # dbctl's slice helpers require an SD; add_ue_with_apn keeps the right
    # APN/DNN but writes the default subscribed slice (sst 1) — a no-sd slice
    # with a different SST won't match until the Rust lifecycle stage writes
    # the subscriber document explicitly (TASKS.md §4).
    podman run --rm --pod "$POD" vx5g-core \
      open5gs-dbctl add_ue_with_apn "$imsi" "$key" "$opc" "$apn" >/dev/null
    echo "subscriber $imsi (apn=$apn sst=$sst — no sd: default subscribed slice, see TASKS §4)"
  fi
done

echo "pod up — check: podman ps --pod --filter pod=$POD ; logs: podman logs $POD-amf"
echo "RAN:"
echo "  podman run -d --pod $POD --name $POD-gnb -v $CFG:/etc/vx5g:ro,Z vx5g-ran nr-gnb -c /etc/vx5g/ueransim/gnb.yaml"
echo "  podman run -d --pod $POD --name $POD-ue-<slice> --cap-add=NET_ADMIN --device /dev/net/tun \\"
echo "    -v $CFG:/etc/vx5g:ro,Z vx5g-ran nr-ue -c /etc/vx5g/ueransim/ue-<slice>.yaml"
