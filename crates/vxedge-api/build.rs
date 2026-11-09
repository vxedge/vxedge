fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "../../proto/vxedge/v1/common.proto",
        "../../proto/vxedge/v1/jobs.proto",
        "../../proto/vxedge/v1/events.proto",
        "../../proto/vxedge/v1/telemetry.proto",
        "../../proto/vxedge/v1/lifecycle.proto",
        "../../proto/vxedge/v1/runs.proto",
        "../../proto/vxedge/v1/workspace.proto",
        "../../proto/vxedge/v1/package.proto",
    ];
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let descriptors = out.join("vxedge_descriptor.bin");
    tonic_prost_build::configure()
        .file_descriptor_set_path(&descriptors)
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, &["../../proto"])?;
    // Serde impls in the canonical Protobuf JSON mapping (enum values by name, lowerCamelCase
    // fields), behind the `serde` feature: what an agent-facing client returns as typed JSON.
    // Fields at their default value are emitted too, so an agent reads `"available": false` or
    // `"runs": []` instead of inferring meaning from a missing key.
    pbjson_build::Builder::new()
        .register_descriptors(&std::fs::read(&descriptors)?)?
        .emit_fields()
        .build(&[".vxedge"])?;
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    Ok(())
}
