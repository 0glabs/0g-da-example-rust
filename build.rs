// Sticking to ethers, we will move to alloy wholesale once we know our needed features set and its
// availibility on alloy.
//
//use foundry_compilers::{
//    remappings::Remapping, resolver::print, utils, Project, ProjectPathsConfig,
//};
use anyhow::Result;
use tonic_build;

//  Build GRPC bindings for ZGDA
//
//  # Arguements:
//  *   proto_file_path: Path to the ZGDA GRPC IDL file
//
//  # Examples:
//  *  build_zgda("./nucleus/contracts").expect("Compilation failed");
fn build_zgda(proto_file_path: &str) -> Result<()> {
    // Build GRPC binding for ZGDA
    tonic_build::compile_protos(proto_file_path)?;
    Ok(())
}

fn main() -> Result<()> {
    // Build ZGDA RPC:
    // https://github.com/zero-gravity-labs/zerog-data-avail/blob/main/api/proto/disperser/disperser.proto
    build_zgda("src/disperser.proto")?;
    Ok(())
}
