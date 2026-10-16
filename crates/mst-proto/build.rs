fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/agent.proto");
    tonic_prost_build::configure()
        .compile_protos(&["../../proto/agent.proto"], &["../../proto"])?;
    Ok(())
}
