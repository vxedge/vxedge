//! Generated Rust bindings for vxAPI. The Protobuf IDL under `proto/vxedge/v1/`
//! is the authoritative contract; this crate only re-exports what
//! `tonic-build` generates from it.

pub mod v1 {
    tonic::include_proto!("vxedge.v1");
    // Canonical Protobuf JSON for every message and enum: enum values serialize by name.
    #[cfg(feature = "serde")]
    include!(concat!(env!("OUT_DIR"), "/vxedge.v1.serde.rs"));
}
