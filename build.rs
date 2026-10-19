const PROTOS: &[&str] = &["proto/cardinal.proto", "proto/raft.proto"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build without a system `protoc`: use the vendored binary unless the user points
    // PROTOC at their own (same variable prost-build honours).
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        // SAFETY: build scripts are single-threaded at this point.
        unsafe { std::env::set_var("PROTOC", protoc) };
    }

    for proto in PROTOS {
        println!("cargo:rerun-if-changed={proto}");
    }
    println!("cargo:rerun-if-env-changed=PROTOC");

    tonic_prost_build::configure().compile_protos(PROTOS, &["proto"])?;
    Ok(())
}
