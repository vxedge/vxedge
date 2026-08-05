//! vxedge domain library.
//!
//! Manifests (YAML, schema in `docs/ARCHITECTURE.md` §6) are the sole source
//! of truth. This crate owns their model + validation, and will own the
//! reconciler and provisioning backends (libvirt, rtnetlink/OVSDB, containers,
//! Open5GS, traffic) as they land.

pub mod fiveg;
pub mod gateway;
pub mod model;
pub mod package;
pub mod registry;
pub mod scaffold;
pub mod services;
pub mod sonic;
pub mod status;
pub mod traffic;
pub mod workspace;
pub mod zones;

pub use model::{Experiment as ModelExperiment, ModelError, build as build_model};
pub use package::{DiscoverError, DiscoveredWorkspace, PackageDoc, discover};
pub use scaffold::{ScaffoldError, Template, WorkspacePlan};
pub use workspace::{Workspace, WorkspaceError, WorkspaceValidationError};
