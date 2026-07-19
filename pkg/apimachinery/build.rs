//! Compile the vendored Kubernetes `.proto` files into a single
//! FileDescriptorSet, which the `protobuf` module loads at runtime with
//! prost-reflect to decode/encode the k8s wire format by GVK.
//!
//! We only need the descriptor set (not generated Rust structs), so this shells
//! out to `protoc --descriptor_set_out` rather than using prost-build. protoc is
//! already required by the workspace (CRI gRPC via tonic-build).

use std::path::Path;
use std::process::Command;

/// Proto files to include, relative to `protos/`. Their transitive imports are
/// pulled in by `--include_imports`. Add a group's `generated.proto` here to
/// extend wire-format coverage to that group.
const PROTOS: &[&str] = &[
    "k8s.io/api/core/v1/generated.proto",
    "k8s.io/api/apps/v1/generated.proto",
    "k8s.io/api/batch/v1/generated.proto",
    "k8s.io/api/coordination/v1/generated.proto",
    "k8s.io/api/discovery/v1/generated.proto",
    "k8s.io/api/storage/v1/generated.proto",
    "k8s.io/api/rbac/v1/generated.proto",
    "k8s.io/api/networking/v1/generated.proto",
    "k8s.io/apimachinery/pkg/apis/meta/v1/generated.proto",
];

fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let desc_path = Path::new(&out_dir).join("k8s_descriptor.bin");
    let protoc = std::env::var("PROTOC").unwrap_or_else(|_| "protoc".to_string());

    let mut cmd = Command::new(&protoc);
    cmd.arg("--include_imports")
        .arg(format!("--descriptor_set_out={}", desc_path.display()))
        .arg("-I")
        .arg("protos");
    for p in PROTOS {
        cmd.arg(p);
    }

    let status = cmd.status().unwrap_or_else(|e| {
        panic!("failed to run protoc ({protoc}): {e}. protoc is required to build the k8s protobuf codec.")
    });
    assert!(status.success(), "protoc failed to build the descriptor set");

    println!("cargo:rerun-if-changed=protos");
    println!("cargo:rerun-if-changed=build.rs");
}
