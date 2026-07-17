# vx5g containers — the 5G stack, off the host

Hybrid split (ARCHITECTURE §15 containerization policy): the 5G stack — Open5GS
NFs, UERANSIM, MongoDB — runs containerized under **Podman**; the measured path
(kernel data plane, KVM/libvirt zones, `vxedged`, agents) stays on the host so
measurements and frozen-campaign comparability are untouched by container
networking. All containers of the stack share one pod's network namespace, so
`FiveGPlan`'s loopback addressing works unchanged in or out of the pod.

## Images

| Image | Contents | Build |
|---|---|---|
| `vx5g-core` | Open5GS 2.8.x (PPA — the version the renderer is validated against), every NF | `podman build -t vx5g-core -f Containerfile.open5gs .` |
| `vx5g-ran` | UERANSIM (pinned tag, source build): `nr-gnb`, `nr-ue`, `nr-cli`, plus `vxprobe` (staged from the sibling `vxagents` repo — an H2 campaign's reflector runs *inside* a UE's own pod) | `./build-vx5g-ran.sh` (stages the `vxprobe` binary into this build context first — `vxagents` is a separate repo podman can't reach directly; do not `podman build` this Containerfile on its own) |

Reproducibility: image digests are the version pin (§13). When the lifecycle
stage lands, the digests used by a run get recorded into results metadata.

## Host prerequisites (the honest, minimal list)

- Podman (rootless works for the core; UPF/UE containers need
  `--cap-add=NET_ADMIN --device /dev/net/tun`, granted per container).
- `modprobe sctp` once per boot — UERANSIM's NGAP uses kernel SCTP, which is
  not namespaced; nothing else from UERANSIM touches the host.
- No Open5GS/Mongo packages on the host. If the PPA install predates this
  (auto-started ~20 systemd services): `systemctl disable --now 'open5gs-*'`,
  purge once the pod path is proven.

## Dev/validation flow

```bash
cargo run -p vxedge-core --example render_fiveg -- <manifest.yaml> /tmp/vx5g-cfg
./pod-up.sh /tmp/vx5g-cfg          # mongo + nrf + rendered amf/nssf/smf*/upf*
podman ps --pod --filter pod=vx5g
./pod-up.sh down
```

`pod-up.sh` is the reference for the `vxedged` fiveg lifecycle stage, not the
product path. Known gaps (TASKS.md §4): AUSF/UDM/UDR + subscriber insertion
(`open5gs-dbctl`, keys matching the rendered `ue-*.yaml`) are lifecycle-stage
work — until then UE registration cannot complete end to end.
