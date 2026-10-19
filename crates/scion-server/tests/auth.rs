//! Every control and object RPC needs a valid token; health checks need none.

mod common;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::Bearer;
use scion_server::TokenKey;
use scion_store::MemoryStore;
use tonic::Code;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::service::Interceptor;
use tonic::transport::Channel;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;
use tonic_prost::ProstCodec;

/// `/scion.v1.<Service>/<Method>` of every RPC in the proto file.
fn every_rpc() -> Vec<String> {
    let proto = include_str!("../../scion-proto/proto/scion/v1/scion.proto");
    let mut service = "";
    let mut rpcs = Vec::new();
    for line in proto.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("service ") {
            service = rest.trim_end_matches(" {");
        } else if let Some(rest) = line.strip_prefix("rpc ") {
            let method = &rest[..rest.find('(').unwrap()];
            rpcs.push(format!("/scion.v1.{service}/{method}"));
        }
    }
    rpcs
}

/// Call `path` with an empty message and `bearer`; returns the status code.
async fn call(channel: &Channel, path: &str, mut bearer: Bearer) -> Code {
    let mut grpc = tonic::client::Grpc::new(channel.clone());
    grpc.ready().await.unwrap();
    let req = bearer.call(tonic::Request::new(())).unwrap();
    let path = PathAndQuery::try_from(path.to_owned()).unwrap();
    match grpc
        .unary::<(), (), _>(req, path, ProstCodec::default())
        .await
    {
        Ok(_) => Code::Ok,
        Err(st) => {
            if st.code() == Code::Unauthenticated {
                assert!(st.message().contains("scn login"), "{}", st.message());
            }
            st.code()
        }
    }
}

#[tokio::test]
async fn every_rpc_needs_a_valid_token() {
    let store = Arc::new(MemoryStore::new());
    let channel = common::serve(store.clone(), store).await;
    let key = common::key();
    let past = SystemTime::now() - Duration::from_secs(1);
    let soon = SystemTime::now() + Duration::from_secs(3600);
    let other = TokenKey::from_hex(&"a7".repeat(32)).unwrap();
    let good = common::token();
    let mut altered = good.clone().into_bytes();
    altered[4] = if altered[4] == b'0' { b'1' } else { b'0' };
    let refused = [
        ("no token", Bearer(None)),
        ("not bearer", Bearer(Some(good.parse().unwrap()))),
        ("not hex", Bearer::of("not-a-token")),
        ("altered", Bearer::of(&String::from_utf8(altered).unwrap())),
        ("expired", Bearer::of(&key.issue("sp:test", past).unwrap())),
        (
            "other key",
            Bearer::of(&other.issue("sp:test", soon).unwrap()),
        ),
    ];

    let rpcs = every_rpc();
    assert_eq!(rpcs.len(), 19, "{rpcs:?}");
    for rpc in &rpcs {
        for (why, bearer) in &refused {
            assert_eq!(
                call(&channel, rpc, bearer.clone()).await,
                Code::Unauthenticated,
                "{rpc} with {why}"
            );
        }
        // Past the boundary the empty request fails on its contents.
        let code = call(&channel, rpc, Bearer::of(&good)).await;
        assert_ne!(code, Code::Unauthenticated, "{rpc} with a valid token");
        assert_ne!(code, Code::Unimplemented, "{rpc} with a valid token");
    }

    // Health checks stay open, for load balancers and compose.
    HealthClient::new(channel)
        .check(HealthCheckRequest::default())
        .await
        .unwrap();
}
