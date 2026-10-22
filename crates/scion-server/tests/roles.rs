//! Each RPC needs a role: `reader` for reads and the object plane, `writer`
//! for creating changesets, merges, reverts and change requests, `admin` for
//! `MoveRef` and managing grants, server admin for `InitRepo`, and only a
//! token for `WhoAmI`. A caller without it gets
//! `PERMISSION_DENIED` before the request is looked at; a caller with it gets
//! past the check (the request may still fail on its contents).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::Bearer;
use scion_derived::test_util::{REPO, changeset};
use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::{
    BlameRequest, CreateChangesetRequest, DiffRequest, FindLandingRequest, GetBlobRequest,
    GetChangeRequestRequest, GetObjectsRequest, GetRefRequest, GetTreeRequest, GrantRequest,
    HasBlobsRequest, InitRepoRequest, ListChangeRequestsRequest, ListGrantsRequest, LogRequest,
    MergeBaseRequest, MergeRequest, MoveRefRequest, PathLogRequest, PutBlobRequest,
    PutBlobsRequest, PutTreeRequest, RebaseRequest, RevertRequest, RevokeRequest,
    SubmitChangeRequestRequest, WhoAmIRequest,
};
use scion_store::{MemoryStore, MetadataStore, Role, SERVER_GRANTS};
use scion_types::{Encode, Tree};
use tonic::Code;
use tonic::transport::Channel;

/// What an RPC needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    Repo(Role),
    ServerAdmin,
    Token,
}

const RPCS: &[(&str, Need)] = &[
    ("GetTree", Need::Repo(Role::Reader)),
    ("GetBlob", Need::Repo(Role::Reader)),
    ("PutBlob", Need::Repo(Role::Reader)),
    ("PutTree", Need::Repo(Role::Reader)),
    ("GetObjects", Need::Repo(Role::Reader)),
    ("HasBlobs", Need::Repo(Role::Reader)),
    ("PutBlobs", Need::Repo(Role::Reader)),
    ("InitRepo", Need::ServerAdmin),
    ("GetRef", Need::Repo(Role::Reader)),
    ("CreateChangeset", Need::Repo(Role::Writer)),
    ("MoveRef", Need::Repo(Role::Admin)),
    ("Diff", Need::Repo(Role::Reader)),
    ("Log", Need::Repo(Role::Reader)),
    ("PathLog", Need::Repo(Role::Reader)),
    ("MergeBase", Need::Repo(Role::Reader)),
    ("Rebase", Need::Repo(Role::Writer)),
    ("Merge", Need::Repo(Role::Writer)),
    ("Revert", Need::Repo(Role::Writer)),
    ("Blame", Need::Repo(Role::Reader)),
    ("SubmitChangeRequest", Need::Repo(Role::Writer)),
    ("GetChangeRequest", Need::Repo(Role::Reader)),
    ("ListChangeRequests", Need::Repo(Role::Reader)),
    ("FindLanding", Need::Repo(Role::Reader)),
    ("Grant", Need::Repo(Role::Admin)),
    ("Revoke", Need::Repo(Role::Admin)),
    ("ListGrants", Need::Repo(Role::Admin)),
    ("WhoAmI", Need::Token),
];

/// Call `rpc` on `REPO` (a new repository for `InitRepo`) as `bearer`;
/// returns the status code.
#[allow(clippy::too_many_lines)] // one arm per RPC
async fn call(channel: &Channel, rpc: &str, bearer: Bearer, n: usize) -> Code {
    let mut control = ControlServiceClient::with_interceptor(channel.clone(), bearer.clone());
    let mut objects = ObjectServiceClient::with_interceptor(channel.clone(), bearer);
    let repo = || REPO.to_owned();
    let id = vec![0u8; 32];
    let child = changeset(vec![], BTreeMap::new(), &format!("change {n}"));
    let result = match rpc {
        "GetTree" => objects
            .get_tree(GetTreeRequest {
                id: id.clone(),
                repo: repo(),
            })
            .await
            .map(drop),
        "GetBlob" => objects
            .get_blob(GetBlobRequest {
                id: id.clone(),
                repo: repo(),
            })
            .await
            .map(drop),
        "PutBlob" => objects
            .put_blob(PutBlobRequest {
                data: b"x".to_vec(),
                repo: repo(),
            })
            .await
            .map(drop),
        "PutTree" => objects
            .put_tree(PutTreeRequest {
                tree: Tree::empty().encode_to_vec(),
                repo: repo(),
            })
            .await
            .map(drop),
        "GetObjects" => objects
            .get_objects(GetObjectsRequest {
                repo: repo(),
                ..GetObjectsRequest::default()
            })
            .await
            .map(drop),
        "HasBlobs" => objects
            .has_blobs(HasBlobsRequest {
                repo: repo(),
                ids: vec![id.clone()],
            })
            .await
            .map(drop),
        "PutBlobs" => objects
            .put_blobs(futures_util::stream::iter([PutBlobsRequest {
                repo: repo(),
                data: b"x".to_vec(),
            }]))
            .await
            .map(drop),
        "InitRepo" => control
            .init_repo(InitRepoRequest {
                repo: format!("new-{n}"),
            })
            .await
            .map(drop),
        "GetRef" => control
            .get_ref(GetRefRequest {
                repo: repo(),
                name: "main".into(),
            })
            .await
            .map(drop),
        "CreateChangeset" => control
            .create_changeset(CreateChangesetRequest {
                changeset: child.encode_to_vec(),
            })
            .await
            .map(drop),
        "MoveRef" => control
            .move_ref(MoveRefRequest {
                repo: repo(),
                name: format!("release/{n}"),
                target: child.id().as_bytes().to_vec(),
                expected_version: None,
            })
            .await
            .map(drop),
        "Diff" => control
            .diff(DiffRequest {
                repo: repo(),
                new: id.clone(),
                ..Default::default()
            })
            .await
            .map(drop),
        "Log" => control
            .log(LogRequest {
                repo: repo(),
                head: id.clone(),
                limit: 1,
            })
            .await
            .map(drop),
        "PathLog" => control
            .path_log(PathLogRequest {
                repo: repo(),
                head: id.clone(),
                path: "a".into(),
                limit: 1,
            })
            .await
            .map(drop),
        "MergeBase" => control
            .merge_base(MergeBaseRequest {
                repo: repo(),
                a: id.clone(),
                b: id.clone(),
            })
            .await
            .map(drop),
        "Rebase" => control
            .rebase(RebaseRequest {
                repo: repo(),
                head: id.clone(),
                onto: id.clone(),
            })
            .await
            .map(drop),
        "Merge" => control
            .merge(MergeRequest {
                repo: repo(),
                ours: id.clone(),
                theirs: id.clone(),
                ..Default::default()
            })
            .await
            .map(drop),
        "Revert" => control
            .revert(RevertRequest {
                repo: repo(),
                head: id.clone(),
                changeset: id.clone(),
                ..Default::default()
            })
            .await
            .map(drop),
        "Blame" => control
            .blame(BlameRequest {
                repo: repo(),
                changeset: id.clone(),
                path: "a".into(),
            })
            .await
            .map(drop),
        "SubmitChangeRequest" => control
            .submit_change_request(SubmitChangeRequestRequest {
                repo: repo(),
                head: id.clone(),
                wait: false,
            })
            .await
            .map(drop),
        "GetChangeRequest" => control
            .get_change_request(GetChangeRequestRequest {
                repo: repo(),
                id: 1,
                wait: false,
            })
            .await
            .map(drop),
        "ListChangeRequests" => control
            .list_change_requests(ListChangeRequestsRequest {
                repo: repo(),
                limit: 1,
                ..Default::default()
            })
            .await
            .map(drop),
        "FindLanding" => control
            .find_landing(FindLandingRequest {
                repo: repo(),
                changesets: vec![id.clone()],
            })
            .await
            .map(drop),
        "Grant" => control
            .grant(GrantRequest {
                repo: repo(),
                grant: Some(scion_proto::v1::AccessGrant {
                    principal: format!("sp:grantee-{n}"),
                    path_prefix: String::new(),
                    role: "reader".into(),
                }),
            })
            .await
            .map(drop),
        "Revoke" => control
            .revoke(RevokeRequest {
                repo: repo(),
                principal: format!("sp:grantee-{n}"),
                path_prefix: String::new(),
            })
            .await
            .map(drop),
        "ListGrants" => control
            .list_grants(ListGrantsRequest { repo: repo() })
            .await
            .map(drop),
        "WhoAmI" => control
            .who_am_i(WhoAmIRequest { repo: repo() })
            .await
            .map(drop),
        other => panic!("no call for {other}"),
    };
    result.map_or_else(|st| st.code(), |()| Code::Ok)
}

#[tokio::test]
async fn every_rpc_as_each_role() {
    let store = Arc::new(MemoryStore::with_repo(REPO));
    let channel = common::serve(store.clone(), store.clone()).await;
    // `sp:test`, which `serve` makes an admin, is not one of these.
    let callers: [(&str, Option<Role>, bool); 5] = [
        ("sp:nobody", None, false),
        ("sp:reader", Some(Role::Reader), false),
        ("sp:writer", Some(Role::Writer), false),
        ("sp:admin", Some(Role::Admin), false),
        ("sp:server", None, true),
    ];
    for (principal, role, server) in callers {
        if let Some(role) = role {
            store
                .put_grant(REPO, &common::grant(principal, role))
                .await
                .unwrap();
        }
        if server {
            store
                .put_grant(SERVER_GRANTS, &common::grant(principal, Role::Admin))
                .await
                .unwrap();
        }
    }
    // A role in another repository counts for nothing here.
    store.create_repo("elsewhere").await.unwrap();
    store
        .put_grant("elsewhere", &common::grant("sp:nobody", Role::Admin))
        .await
        .unwrap();

    let soon = SystemTime::now() + Duration::from_secs(3600);
    let proto = include_str!("../../scion-proto/proto/scion/v1/scion.proto");
    // Login needs no role: it is how a principal gets a token.
    let login = proto.split("service LoginService").nth(1).unwrap();
    let rpcs = proto.matches("  rpc ").count() - login.matches("  rpc ").count();
    assert_eq!(rpcs, RPCS.len(), "a row per RPC");
    let mut n = 0;
    for (rpc, need) in RPCS {
        for (principal, role, server) in callers {
            n += 1;
            let bearer = Bearer::of(&common::key().issue(principal, soon).unwrap());
            let allowed = match need {
                Need::ServerAdmin => server,
                Need::Repo(need) => role >= Some(*need),
                Need::Token => true,
            };
            let code = call(&channel, rpc, bearer, n).await;
            let succeeds = [
                "InitRepo",
                "PutBlob",
                "PutTree",
                "HasBlobs",
                "PutBlobs",
                "CreateChangeset",
                "Grant",
                "ListGrants",
                "WhoAmI",
            ];
            if allowed && succeeds.contains(rpc) {
                assert_eq!(code, Code::Ok, "{rpc} as {principal}");
            } else if allowed {
                assert!(
                    !matches!(code, Code::PermissionDenied | Code::Unauthenticated),
                    "{rpc} as {principal}: {code:?}"
                );
            } else {
                assert_eq!(code, Code::PermissionDenied, "{rpc} as {principal}");
            }
        }
    }
}
