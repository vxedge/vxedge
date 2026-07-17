//! vxedge domain library.
//!
//! Manifests (YAML, schema in `docs/ARCHITECTURE.md` §6) are the sole source
//! of truth. This crate owns their model + validation, and will own the
//! reconciler and provisioning backends (libvirt, rtnetlink/OVSDB, containers,
//! Open5GS, traffic) as they land.

pub mod fiveg;
pub mod gateway;
pub mod manifest;
pub mod services;
pub mod traffic;
pub mod zones;

pub use manifest::{ExperimentManifest, ValidationError};
