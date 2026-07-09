fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "../../proto/vxedge/v1/common.proto",
        "../../proto/vxedge/v1/jobs.proto",
        "../../proto/vxedge/v1/events.proto",
        "../../proto/vxedge/v1/telemetry.proto",
        "../../proto/vxedge/v1/lifecycle.proto",
    ];
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, &["../../proto"])?;
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    Ok(())
}
