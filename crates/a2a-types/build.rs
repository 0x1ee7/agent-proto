//! Generates the protobuf message types and their proto3-JSON serde impls.
//!
//! Sources: the vendored upstream `a2a.proto` and googleapis imports under `proto/`, and the
//! A2A-Bus extension protos under the repository's `proto/a2abus/v1/`. Parsing uses protox
//! (pure Rust, no `protoc`), so the build needs no external tools.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const VENDORED_FILES: [&str; 3] = [
    "a2a.proto",
    "google/rpc/status.proto",
    "google/rpc/error_details.proto",
];

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let vendored = manifest_dir.join("proto");
    let bus_dir = manifest_dir.join("../../proto/a2abus/v1");

    let mut bus_files = bus_proto_names(&bus_dir)?;
    bus_files.sort();

    println!("cargo:rerun-if-changed={}", vendored.display());
    println!("cargo:rerun-if-changed={}", bus_dir.display());
    for name in bus_files.iter() {
        println!("cargo:rerun-if-changed={}", bus_dir.join(name).display());
    }

    let mut open: Vec<String> = VENDORED_FILES.iter().map(|s| (*s).to_owned()).collect();
    open.extend(bus_files);

    let mut compiler = protox::Compiler::new([vendored.as_path(), bus_dir.as_path()])?;
    compiler.include_imports(true);
    compiler.open_files(&open)?;
    let descriptor_set = compiler.file_descriptor_set();

    // Well-known types resolve to pbjson_types, which carries serde impls for them. Without
    // compile_well_known_types, prost-build would also map .google.protobuf to prost_types and
    // reject the explicit extern path as a duplicate.
    prost_build::Config::new()
        .compile_well_known_types()
        .bytes(["."])
        .extern_path(".google.protobuf", "::pbjson_types")
        .compile_fds(descriptor_set.clone())?;

    let mut pbjson = pbjson_build::Builder::new();
    for file in descriptor_set.file.iter() {
        pbjson.register_file_descriptor(file.clone());
    }
    pbjson.build(&[".lf.a2a.v1", ".a2abus.v1", ".google.rpc"])?;

    Ok(())
}

/// Lists the `*.proto` file names directly inside `dir`, relative to it.
fn bus_proto_names(dir: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".proto") {
            names.push(name);
        }
    }
    if names.is_empty() {
        return Err(format!("no .proto files found in {}", dir.display()).into());
    }
    Ok(names)
}
