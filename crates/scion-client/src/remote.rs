//! The connection to the server. In the MVP both planes go to `scion-server`
//! directly; the object plane moves to the nearest cache tier in Phase 4.

use std::time::Duration;

use scion_proto::v1::control_service_client::ControlServiceClient;
use scion_proto::v1::object_service_client::ObjectServiceClient;
use scion_proto::v1::{
    BlameRequest, CreateChangesetRequest, DiffRequest, GetBlobRequest, GetRefRequest,
    GetTreeRequest, LogRequest, LogResponse, MoveRefRequest, PathLogRequest, PutBlobRequest,
};
use scion_types::{
    BlobId, Changeset, ChangesetId, Decode, Encode, FileType, RepoPath, Tree, TreeId,
};
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

/// One side of a [`DiffEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEntry {
    pub id: BlobId,
    pub kind: FileType,
    pub size: u64,
}

/// A file that differs between two changesets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: RepoPath,
    /// `None` for an added file.
    pub old: Option<FileEntry>,
    /// `None` for a deleted file.
    pub new: Option<FileEntry>,
}

/// Consecutive lines last changed by one changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlameRun {
    pub changeset: ChangesetId,
    pub lines: u64,
}

fn changeset_id(bytes: &[u8], what: &'static str) -> Result<ChangesetId, RemoteError> {
    ChangesetId::try_from_slice(bytes).map_err(|_| RemoteError::Malformed(what))
}

fn changesets(resp: &LogResponse) -> Result<Vec<Changeset>, RemoteError> {
    resp.changesets
        .iter()
        .map(|b| Changeset::decode(b).map_err(|_| RemoteError::Malformed("changeset")))
        .collect()
}

fn file_entry(f: &scion_proto::v1::FileEntry) -> Result<FileEntry, RemoteError> {
    Ok(FileEntry {
        id: BlobId::try_from_slice(&f.id).map_err(|_| RemoteError::Malformed("blob id"))?,
        kind: match scion_proto::v1::FileType::try_from(f.kind) {
            Ok(scion_proto::v1::FileType::Regular) => FileType::Regular,
            Ok(scion_proto::v1::FileType::Executable) => FileType::Executable,
            Ok(scion_proto::v1::FileType::Symlink) => FileType::Symlink,
            Err(_) => return Err(RemoteError::Malformed("file type")),
        },
        size: f.size,
    })
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
            target: changeset_id(&resp.target, "ref target")?,
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

    /// First-parent history of `head`, newest first, at most `limit` long.
    pub async fn log(
        &mut self,
        repo: &str,
        head: ChangesetId,
        limit: u32,
    ) -> Result<Vec<Changeset>, RemoteError> {
        let resp = self
            .control
            .log(LogRequest {
                repo: repo.to_owned(),
                head: head.as_bytes().to_vec(),
                limit,
            })
            .await?
            .into_inner();
        changesets(&resp)
    }

    /// First-parent history of `head` that touched `path`, newest first.
    pub async fn path_log(
        &mut self,
        repo: &str,
        head: ChangesetId,
        path: &RepoPath,
        limit: u32,
    ) -> Result<Vec<Changeset>, RemoteError> {
        let resp = self
            .control
            .path_log(PathLogRequest {
                repo: repo.to_owned(),
                head: head.as_bytes().to_vec(),
                path: path.as_str().to_owned(),
                limit,
            })
            .await?
            .into_inner();
        changesets(&resp)
    }

    /// Files that differ between `old` (the empty tree if `None`) and `new`,
    /// sorted by path.
    pub async fn diff(
        &mut self,
        repo: &str,
        old: Option<ChangesetId>,
        new: ChangesetId,
    ) -> Result<Vec<DiffEntry>, RemoteError> {
        let resp = self
            .control
            .diff(DiffRequest {
                repo: repo.to_owned(),
                old: old.map(|id| id.as_bytes().to_vec()),
                new: new.as_bytes().to_vec(),
            })
            .await?
            .into_inner();
        resp.entries
            .into_iter()
            .map(|e| {
                Ok(DiffEntry {
                    path: RepoPath::new(e.path).map_err(|_| RemoteError::Malformed("path"))?,
                    old: e.old.as_ref().map(file_entry).transpose()?,
                    new: e.new.as_ref().map(file_entry).transpose()?,
                })
            })
            .collect()
    }

    /// Line attribution of the file at `path` in `changeset`.
    pub async fn blame(
        &mut self,
        repo: &str,
        changeset: ChangesetId,
        path: &RepoPath,
    ) -> Result<Vec<BlameRun>, RemoteError> {
        let resp = self
            .control
            .blame(BlameRequest {
                repo: repo.to_owned(),
                changeset: changeset.as_bytes().to_vec(),
                path: path.as_str().to_owned(),
            })
            .await?
            .into_inner();
        resp.runs
            .into_iter()
            .map(|r| {
                Ok(BlameRun {
                    changeset: changeset_id(&r.changeset, "blame changeset")?,
                    lines: r.lines,
                })
            })
            .collect()
    }

    /// Upload one blob; returns its id.
    pub async fn put_blob(&mut self, data: Vec<u8>) -> Result<BlobId, RemoteError> {
        let resp = self
            .objects
            .put_blob(PutBlobRequest { data })
            .await?
            .into_inner();
        BlobId::try_from_slice(&resp.id).map_err(|_| RemoteError::Malformed("blob id"))
    }

    /// Create `cs` on the server; its blobs must be uploaded first.
    pub async fn create_changeset(&mut self, cs: &Changeset) -> Result<ChangesetId, RemoteError> {
        let resp = self
            .control
            .create_changeset(CreateChangesetRequest {
                changeset: cs.encode_to_vec(),
            })
            .await?
            .into_inner();
        changeset_id(&resp.id, "changeset id")
    }

    /// Create server ref `name` at `target`; `ALREADY_EXISTS` if it exists.
    /// Returns its version.
    pub async fn create_ref(
        &mut self,
        repo: &str,
        name: &str,
        target: ChangesetId,
    ) -> Result<u64, RemoteError> {
        Ok(self
            .control
            .move_ref(MoveRefRequest {
                repo: repo.to_owned(),
                name: name.to_owned(),
                target: target.as_bytes().to_vec(),
                expected_version: None,
            })
            .await?
            .into_inner()
            .version)
    }

    /// Move server ref `name` to `target` if it is still at
    /// `expected_version` and `target` descends from it; returns the new
    /// version.
    pub async fn move_ref(
        &mut self,
        repo: &str,
        name: &str,
        target: ChangesetId,
        expected_version: u64,
    ) -> Result<u64, RemoteError> {
        Ok(self
            .control
            .move_ref(MoveRefRequest {
                repo: repo.to_owned(),
                name: name.to_owned(),
                target: target.as_bytes().to_vec(),
                expected_version: Some(expected_version),
            })
            .await?
            .into_inner()
            .version)
    }
}
