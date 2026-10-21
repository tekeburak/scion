//! The checks behind the fuzz targets in `crates/scion-sim/fuzz`: one per
//! boundary that takes untrusted bytes. Each takes the fuzzer's input,
//! never fails on it, and panics only when a property breaks, so the same
//! functions replay the checked-in corpus under a stable `cargo test`.
//!
//! - [`changeset`], [`tree`]: canonical decoding is strict, so whatever
//!   decodes re-encodes to the same bytes and one value has one id.
//! - [`history_page`], [`blame_node`]: derived objects read back from the
//!   object store decode or are refused, and a decoded blame node survives
//!   its own round trip.
//! - [`token`]: a token that verifies is the one the server would issue for
//!   its principal and expiry; the input is checked as it arrives and, so
//!   the fuzzer reaches past the MAC, signed with the run's key.
//! - [`diff3`]: the three-way merge returns, and a conflict carries markers.
//! - [`request`]: `CreateChangeset` (canonical bytes) and
//!   `SubmitChangeRequest`, `Merge` and `Revert` (protobuf) reach one
//!   in-process server on the memory stores; every refusal is the client's
//!   (never `INTERNAL`, `UNKNOWN` or `DATA_LOSS`) and nothing stored is
//!   overwritten.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message;
use scion_proto::v1::{
    CreateChangesetRequest, InitRepoRequest, MergeRequest, PutBlobRequest, RevertRequest,
    SubmitChangeRequestRequest,
};
use scion_server::TokenKey;
use scion_store::{Grant, MetadataStore, Role, SERVER_GRANTS};
use scion_types::{Changeset, Decode, Encode, Tree};
use tonic::{Code, Status};

use crate::store::SimStore;
use crate::{Auth, PRINCIPAL, REPO, Rng, Server};

/// A target's check of one input.
pub type Check = fn(&[u8]);

/// Every target, by the name of its `fuzz_targets/<name>.rs` and corpus
/// directory.
pub const TARGETS: &[(&str, Check)] = &[
    ("changeset", changeset),
    ("tree", tree),
    ("history_page", history_page),
    ("blame_node", blame_node),
    ("token", token),
    ("diff3", diff3),
    ("request", request),
];

/// The key [`token`] verifies and signs with.
const KEY: [u8; 32] = [0x51; 32];

/// The time [`token`] checks expiry against, so a run replays.
const NOW: Duration = Duration::from_secs(1_800_000_000);

/// A changeset's canonical bytes.
pub fn changeset(data: &[u8]) {
    if let Ok(cs) = Changeset::decode(data) {
        assert_eq!(cs.encode_to_vec(), data, "{cs:?} re-encodes differently");
    }
}

/// A tree's canonical bytes.
pub fn tree(data: &[u8]) {
    if let Ok(tree) = Tree::decode(data) {
        assert_eq!(
            tree.encode_to_vec(),
            data,
            "{tree:?} re-encodes differently"
        );
    }
}

/// A history page; the first byte picks the node.
pub fn history_page(data: &[u8]) {
    if let Some((index, page)) = data.split_first() {
        let _ = scion_derived::decode::history_node(page, u32::from(*index));
    }
}

/// A blame node.
pub fn blame_node(data: &[u8]) {
    if let Some((node, again)) = scion_derived::decode::blame_node(data) {
        let (back, _) =
            scion_derived::decode::blame_node(&again).expect("an encoded blame node decodes");
        assert_eq!(back, node, "a blame node changes on its round trip");
    }
}

/// A bearer token: the first byte's low bit says whether the rest is the
/// token as sent (0) or the bytes to sign with the run's key (1).
pub fn token(data: &[u8]) {
    let Some((mode, rest)) = data.split_first() else {
        return;
    };
    let token = if mode & 1 == 0 {
        String::from_utf8_lossy(rest).into_owned()
    } else {
        let mac = blake3::keyed_hash(&KEY, rest);
        hex::encode([rest, mac.as_bytes()].concat())
    };
    let key = TokenKey::from_hex(&hex::encode(KEY)).expect("a valid key");
    if let Ok((principal, expiry)) = key.verify(&token, UNIX_EPOCH + NOW) {
        let issued = key.issue(&principal, expiry).expect("a verified principal");
        assert_eq!(issued, token.to_ascii_lowercase(), "{principal} {expiry:?}");
    }
}

/// Three files for a merge: two big-endian `u16` lengths, then base, ours
/// and the rest as theirs.
pub fn diff3(data: &[u8]) {
    let Some((lengths, data)) = data.split_first_chunk::<4>() else {
        return;
    };
    let base_len = usize::from(u16::from_be_bytes([lengths[0], lengths[1]])).min(data.len());
    let (base, data) = data.split_at(base_len);
    let ours_len = usize::from(u16::from_be_bytes([lengths[2], lengths[3]])).min(data.len());
    let (ours, theirs) = data.split_at(ours_len);
    match scion_diff3::merge(base, ours, theirs) {
        scion_diff3::Merged::Clean(_) => {}
        scion_diff3::Merged::Conflicted { content, conflicts } => {
            assert!(conflicts > 0, "a conflicted merge counts no conflict");
            let marker = [b'<'; scion_diff3::MARKER_LEN];
            assert!(
                content.windows(marker.len()).any(|w| w == marker),
                "a conflicted merge has no marker"
            );
        }
    }
}

/// The server [`request`] calls, with `main` at the repository root and
/// the blob [`BLOB`] uploaded, kept for the whole run.
struct Harness {
    store: Arc<SimStore>,
    server: Server,
}

/// The one blob a fuzzed changeset can name without a hole.
pub const BLOB: &[u8] = b"fuzz\n";

thread_local! {
    static HARNESS: Harness = Harness::start();
}

impl Harness {
    fn start() -> Self {
        let store = Arc::new(SimStore::new(Arc::new(Rng::new(0))));
        let key = TokenKey::from_hex(&hex::encode(KEY)).expect("a valid key");
        let token = key
            .issue(PRINCIPAL, SystemTime::now() + Duration::from_secs(86_400))
            .expect("a valid principal");
        let auth = Auth(format!("Bearer {token}").parse().expect("a token is ASCII"));
        let server = Server::start(&store, &key, &auth);
        let admin = Grant {
            principal: PRINCIPAL.into(),
            path_prefix: String::new(),
            role: Role::Admin,
        };
        let mut api = server.api.clone();
        server.rt.block_on(async {
            store
                .inner
                .put_grant(SERVER_GRANTS, &admin)
                .await
                .expect("the memory store answers");
            api.control
                .init_repo(InitRepoRequest { repo: REPO.into() })
                .await
                .expect("the repository is created");
            api.objects
                .put_blob(PutBlobRequest {
                    data: BLOB.to_vec(),
                    repo: REPO.into(),
                })
                .await
                .expect("the blob is stored");
        });
        Self { store, server }
    }
}

/// One request to the server: the first byte picks `CreateChangeset` (the
/// rest is the changeset's canonical bytes), `SubmitChangeRequest`, `Merge`
/// or `Revert` (the rest is the request's protobuf).
pub fn request(data: &[u8]) {
    let Some((rpc, rest)) = data.split_first() else {
        return;
    };
    HARNESS.with(|h| {
        let mut api = h.server.api.clone();
        let reply = h.server.rt.block_on(async {
            match rpc % 4 {
                0 => api
                    .control
                    .create_changeset(CreateChangesetRequest {
                        changeset: rest.to_vec(),
                    })
                    .await
                    .map(drop),
                1 => {
                    let Ok(mut req) = SubmitChangeRequestRequest::decode(rest) else {
                        return Ok(());
                    };
                    // Waiting would park the run for up to 20 seconds.
                    req.wait = false;
                    api.control.submit_change_request(req).await.map(drop)
                }
                2 => match MergeRequest::decode(rest) {
                    Ok(req) => api.control.merge(req).await.map(drop),
                    Err(_) => Ok(()),
                },
                _ => match RevertRequest::decode(rest) {
                    Ok(req) => api.control.revert(req).await.map(drop),
                    Err(_) => Ok(()),
                },
            }
        });
        if let Err(st) = reply {
            assert!(!server_fault(&st), "{st:?}");
        }
        let written = h.store.written.lock().expect("poisoned");
        assert!(
            written.overwrites.is_empty(),
            "overwritten: {:?}",
            written.overwrites
        );
    });
}

/// Whether `st` blames the server rather than the request.
fn server_fault(st: &Status) -> bool {
    matches!(st.code(), Code::Internal | Code::Unknown | Code::DataLoss)
}
