//! The Scion wire protocol: protobuf messages and tonic client/server code for
//! the two services every tier speaks.
//!
//! * `ObjectService` serves immutable trees and blobs by id. `scion-server`,
//!   `scion-cache` and `scion-daemon` all implement it.
//! * `ControlService` is the control plane; only `scion-server` implements it.
//!
//! Protobuf is the *transport* only. Ids are never computed over these
//! messages: trees and changesets travel as their canonical encoding
//! (`scion_types::canonical`), and the receiver hashes those bytes itself.
//! The schema is `proto/scion/v1/scion.proto`.

#![forbid(unsafe_code)]

pub mod trace;

/// Version 1 of the protocol, generated from `proto/scion/v1/scion.proto`.
#[allow(
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic,
    rustdoc::all
)]
pub mod v1 {
    tonic::include_proto!("scion.v1");
}
