use std::{
    env,
    fs,
    path::{Path, PathBuf},
};

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc should exist");
    unsafe {
        env::set_var("PROTOC", protoc);
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let repo_root = manifest_dir.parent().expect("crate must live in workspace root");
    let comet_proto_root = repo_root.join("cometbft/proto");
    let local_proto_root = manifest_dir.join("proto");

    println!("cargo:rerun-if-changed={}", comet_proto_root.display());
    println!("cargo:rerun-if-changed={}", local_proto_root.display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let descriptor_path = out_dir.join("cometbft-abci-descriptor.bin");

    let protos = [
        comet_proto_root.join("cometbft/abci/v1/service.proto"),
        comet_proto_root.join("cometbft/abci/v1/types.proto"),
        comet_proto_root.join("cometbft/crypto/v1/keys.proto"),
        comet_proto_root.join("cometbft/crypto/v1/proof.proto"),
        comet_proto_root.join("cometbft/types/v1/params.proto"),
        comet_proto_root.join("cometbft/types/v1/types.proto"),
        comet_proto_root.join("cometbft/types/v1/validator.proto"),
        comet_proto_root.join("cometbft/version/v1/types.proto"),
    ];

    let proto_paths: Vec<&Path> = protos.iter().map(PathBuf::as_path).collect();

    let mut config = prost_build::Config::new();
    config.file_descriptor_set_path(&descriptor_path);
    config.protoc_arg("--experimental_allow_proto3_optional");

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .build_transport(true)
        .compile_protos_with_config(
            config,
            &proto_paths,
            &[comet_proto_root.as_path(), local_proto_root.as_path()],
        )
        .expect("failed to generate CometBFT protos");

    let descriptor_bytes = fs::read(&descriptor_path).expect("descriptor should be readable");
    fs::write(out_dir.join("cometbft_abci_descriptor.rs"), format_descriptor_bytes(&descriptor_bytes))
        .expect("descriptor helper should be writable");
}

fn format_descriptor_bytes(bytes: &[u8]) -> String {
    let body = bytes
        .iter()
        .map(|byte| byte.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("pub const FILE_DESCRIPTOR_SET: &[u8] = &[{body}];\n")
}
