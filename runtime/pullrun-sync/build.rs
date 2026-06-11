fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let proto_path = std::path::Path::new(&manifest_dir).join("../../proto");
    let proto_path = proto_path.canonicalize().unwrap_or(proto_path);

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(
            &[
                proto_path.join("pullrun/sync.proto").to_string_lossy().to_string(),
            ],
            &[proto_path.to_string_lossy().to_string()],
        )
        .unwrap_or_else(|e| panic!("Failed to compile sync proto: {e}"));
}
