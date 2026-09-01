use std::{error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let proto = "proto/clew/v1/clew.proto";
    println!("cargo:rerun-if-changed={proto}");

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let vendored_include = protoc_bin_vendored::include_path()?;
    let includes = [PathBuf::from("proto"), vendored_include];

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    config.compile_protos(&[proto], &includes)?;
    Ok(())
}
