//! A server on the given stores, and clients that sign in to it.

#![allow(dead_code)] // each test file uses part of it

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use scion_derived::test_util::REPO;
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_server::TokenKey;
use scion_store::{Grant, MetadataStore, ObjectStore, Role, SERVER_GRANTS};
use tokio::net::TcpListener;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

pub type Control = ControlServiceClient<InterceptedService<Channel, Bearer>>;
pub type Objects = ObjectServiceClient<InterceptedService<Channel, Bearer>>;

/// The key every test server signs tokens with.
pub fn key() -> TokenKey {
    TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

/// A token of [`key`] for `sp:test`, valid for an hour.
pub fn token() -> String {
    key()
        .issue("sp:test", SystemTime::now() + Duration::from_secs(3600))
        .unwrap()
}

/// Sends `authorization: <value>` on every request, or nothing.
#[derive(Clone)]
pub struct Bearer(pub Option<MetadataValue<Ascii>>);

impl Bearer {
    /// `Bearer <token>`.
    pub fn of(token: &str) -> Self {
        Self(Some(format!("Bearer {token}").parse().unwrap()))
    }
}

impl Interceptor for Bearer {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(v) = &self.0 {
            req.metadata_mut().insert("authorization", v.clone());
        }
        Ok(req)
    }
}

/// A whole-repository grant of `role` to `principal`.
pub fn grant(principal: &str, role: Role) -> Grant {
    Grant {
        principal: principal.to_owned(),
        path_prefix: String::new(),
        role,
    }
}

/// Serve `meta` and `objects` with [`key`] on a free port; returns a channel
/// to it. `sp:test`, the principal of [`token`], is an admin of the server
/// and of [`REPO`] if it exists.
pub async fn serve(meta: Arc<dyn MetadataStore>, objects: Arc<dyn ObjectStore>) -> Channel {
    let admin = grant("sp:test", Role::Admin);
    meta.put_grant(SERVER_GRANTS, &admin).await.unwrap();
    // Not every test creates REPO.
    let _ = meta.put_grant(REPO, &admin).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(listener, meta, objects, key()));
    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

/// A control client sending [`token`].
pub fn control(channel: Channel) -> Control {
    ControlServiceClient::with_interceptor(channel, Bearer::of(&token()))
}

/// An object client sending [`token`].
pub fn objects(channel: Channel) -> Objects {
    ObjectServiceClient::with_interceptor(channel, Bearer::of(&token()))
}
