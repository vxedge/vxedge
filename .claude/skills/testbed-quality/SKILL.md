---
name: testbed-quality
description: The OEM-adoption quality bar for vxedge — use when designing features, APIs, manifests, docs, or releases anywhere in the workspace. Encodes what makes automotive teams (BMW/Mercedes/Toyota/Tesla/Rivian-class) trust and adopt an external testbed.
---

# Testbed quality — the OEM-adoption bar

An OEM team adopts an external testbed only if it is **reproducible, honest, stable, and
legible in their vocabulary**. Every feature is judged against those four properties.

## 1. Reproducibility invariants (never trade away)

- Manifests are the sole source of truth; every artifact records the **manifest SHA-256**.
- Images/binaries are digest-tagged; `latest` tags are forbidden in manifests.
- `up|down` idempotent — double-apply is a no-op, verified by tests (the netns suite is the
  template).
- Every results archive carries metadata: experiment ID, manifest checksum, tool versions,
  host environment (kernel flavour, pinning, hardware). If a figure can't be regenerated
  from an archive, the archive is incomplete.
- One-command reproduction is the product: `vxedge experiment up --manifest …` must remain
  sufficient.

## 2. Fidelity honesty (credibility is compounding)

- Call the platform an **executable virtual prototype / all-software SDV testbed** — the
  word "twin" is reserved for hardware-in-the-loop parity (phase 4) or explicit vendor-twin
  claims (Snapdragon vSoC, Mcity). Overclaiming once costs more than underclaiming always.
- Known gaps are stated where users will see them: QNX-as-guest inversion, shared-host
  gateway, userspace timestamps, non-PREEMPT_RT kernels. New features add their caveats to
  the same places (ARCHITECTURE §2/§9/§12, results metadata).
- Status output never lies: DOWN when nothing is provisioned, DEGRADED with specifics when
  partial, and a job that failed says FAILED with the real error.

## 3. Interface stability (what teams build on)

- `vxedge.v1` is a contract: additive changes only; breaking changes mean `v2`. CI's
  `buf breaking` gate is non-negotiable.
- Manifest schema changes land in `docs/ARCHITECTURE.md` §6 **first**, then code, then
  boards — one source of truth, no drift.
- CLI verbs keep `up|down|status` semantics forever; new nouns follow the same shape.
- Deprecations get a documented migration path before removal.

## 4. OEM legibility (speak their standards)

- Vehicle signals follow **COVESA VSS**; in-vehicle comms map to Zenoh/uProtocol/DDS; test
  bench boundaries aim at **ASAM OSI/FMI/XIL** (phase 3–4). When adding a domain concept,
  check whether a standard already names it — adopt the standard's name.
- Zonal vocabulary is fixed: the six roles (`front-left` … `gateway`) and E/E terms from
  ARCHITECTURE §3. Don't invent synonyms.
- Every subsystem states its production counterpart (vxgateway ↔ zonal gateway silicon,
  SONiC-VS ↔ production NOS, vSoC/Graviton ↔ cloud virtual ECUs) — that mapping is the
  pitch an OEM engineer repeats internally.

## 5. Supply-chain & security posture

- `cargo deny` (advisories, licenses, sources) green everywhere; Dependabot on every repo.
- Containers digest-pinned and non-root; OTA artifacts signed and verified (vxota).
- No secrets in repos or manifests; anything credential-shaped goes through the environment.
- License hygiene: MIT/Apache-2.0 for our code; never commit licensed vendor artifacts
  (QNX, AAOS) — recipes only.

## 6. Documentation currency (the adoption funnel)

- `docs/STATUS.md` updates ride every progress push; `docs/TESTBED-SETUP.md` is the single
  host-requirements truth ("anything the testbed needs that it doesn't list is a bug in it");
  GETTING-STARTED must always work top-to-bottom on a clean machine.
- A feature isn't done until its board item is checked and its docs are current — same
  change set, not a follow-up.
