//! The connection to the server. In the MVP both planes go to `scion-server`
//! directly; the object plane moves to the nearest cache tier in Phase 4.

use std::time::Duration;

use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::{GetBlobRequest, GetRefRequest, GetTreeRequest};
use scion_types::{BlobId, ChangesetId, Decode, Tree, TreeId};
use tonic::transport::{Channel, Endpoint};

/// Deadline the client attaches to every RPC.
pub const RPC_DEADLINE: Duration = Duration::from_secs(30);

/// Largest message the client accepts; matches the server's per-blob limit.
const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Failure talking to the server.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("cannot connect to {0}: {1}")]
    Connect(String, #[source] tonic::transport::Error),
    #[error(transparent)]
    Rpc(#[from] tonic::Status),
    #[error("server sent a malformed {0}")]
    Malformed(&'static str),
}

/// A ref as the server reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteRef {
    pub target: ChangesetId,
    pub version: u64,
    pub root_tree: TreeId,
}

/// Clients for both planes of one server.
#[derive(Debug, Clone)]
pub struct Remote {
    pub control: ControlServiceClient<Channel>,
    pub objects: ObjectServiceClient<Channel>,
}

impl Remote {
    /// Connect to `server` (e.g. `http://127.0.0.1:7070`).
    pub async fn connect(server: &str) -> Result<Self, RemoteError> {
        let channel = Endpoint::from_shared(server.to_owned())
            .map_err(|e| RemoteError::Connect(server.to_owned(), e))?
            .timeout(RPC_DEADLINE)
            .connect_timeout(RPC_DEADLINE)
            .connect()
            .await
            .map_err(|e| RemoteError::Connect(server.to_owned(), e))?;
        Ok(Self {
            control: ControlServiceClient::new(channel.clone())
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
            objects: ObjectServiceClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        })
    }

    /// Read a server ref.
    pub async fn get_ref(&mut self, repo: &str, name: &str) -> Result<RemoteRef, RemoteError> {
        let resp = self
            .control
            .get_ref(GetRefRequest {
                repo: repo.to_owned(),
                name: name.to_owned(),
            })
            .await?
            .into_inner();
        Ok(RemoteRef {
            target: ChangesetId::try_from_slice(&resp.target)
                .map_err(|_| RemoteError::Malformed("ref target"))?,
            version: resp.version,
            root_tree: TreeId::try_from_slice(&resp.root_tree)
                .map_err(|_| RemoteError::Malformed("root tree id"))?,
        })
    }

    /// Fetch one tree.
    pub async fn get_tree(&mut self, id: TreeId) -> Result<Tree, RemoteError> {
        let resp = self
            .objects
            .get_tree(GetTreeRequest {
                id: id.as_bytes().to_vec(),
            })
            .await?
            .into_inner();
        Tree::decode(&resp.tree).map_err(|_| RemoteError::Malformed("tree"))
    }

    /// Fetch one blob.
    pub async fn get_blob(&mut self, id: BlobId) -> Result<Vec<u8>, RemoteError> {
        Ok(self
            .objects
            .get_blob(GetBlobRequest {
                id: id.as_bytes().to_vec(),
            })
            .await?
            .into_inner()
            .data)
    }
}
