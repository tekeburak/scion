//! Generates the gRPC client and server code from `proto/`. Needs `protoc` on
//! `PATH` (or `PROTOC` set).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::compile_protos("proto/scion/v1/scion.proto")?;
    Ok(())
}
