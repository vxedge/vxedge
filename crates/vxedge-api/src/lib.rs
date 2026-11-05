//! Generated Rust bindings for vxAPI. The Protobuf IDL under `proto/vxedge/v1/`
//! is the authoritative contract; this crate only re-exports what
//! `tonic-build` generates from it.

// Generated code we do not own. clippy 1.99 flags two patterns in it:
// - `double_must_use`: tonic-build puts `#[must_use]` on `async_trait` methods that return
//   an already-`must_use` type;
// - `useless_borrows_in_formatting`: pbjson-build writes `write!(f, "{}", &x)`.
#[allow(clippy::double_must_use, clippy::useless_borrows_in_formatting)]
pub mod v1 {
    tonic::include_proto!("vxedge.v1");
    // Canonical Protobuf JSON for every message and enum: enum values serialize by name.
    #[cfg(feature = "serde")]
    include!(concat!(env!("OUT_DIR"), "/vxedge.v1.serde.rs"));
}
