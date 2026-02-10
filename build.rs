fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/cardinal.proto");
    tonic_prost_build::compile_protos("proto/cardinal.proto")?;
    Ok(())
}