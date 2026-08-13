fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "../../../contracts/proto/edge_node.proto";
    println!("cargo:rerun-if-changed={proto}");
    tonic_prost_build::configure().compile_protos(&[proto], &["../../../contracts/proto"])?;
    Ok(())
}
