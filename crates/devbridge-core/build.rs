fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_file = "../../proto/devbridge.proto";
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto_file], &["../../proto"])?;
    println!("cargo:rerun-if-changed={proto_file}");
    Ok(())
}
