# HTTP/JSON API

`scion-server --web <addr>` serves this API under `/api/v1` on the same address as the web
interface. It mirrors the gRPC `ControlService` and `ObjectService`
(`crates/scion-proto/proto/scion/v1/scion.proto`) for clients that do not speak gRPC. Every route
calls the same server code as its RPC, so roles, directory grants, validation, limits and the
land worker are the same; this document only says how each one is spelled over HTTP.

## Conventions

- **Authentication.** Send `Authorization: Bearer <token>` with a token from `scn login` or
  `scion-server token`. A `GET` without a token reads as the reserved principal `anyone`, so
  it sees public repositories only (those where `anyone` holds `reader`, see
  `docs/DATA_MODEL.md`). With a token, a read holds the caller's grants and `anyone`'s, as on
  the web pages. Every other method needs a token. The gRPC services do not read `anyone`'s
  grants.
- **Ids** are 64 lowercase hex digits (the 32-byte BLAKE3 digest). Paths are repository paths
  as in `docs/DATA_MODEL.md` (no leading `/`, no `.` or `..`).
- **Bodies** are JSON, except blob bytes, which travel raw (`application/octet-stream`). A
  blob may be up to 64 MiB (67108864 bytes), the per-blob limit `scion_types::MAX_BLOB_BYTES`
  that gRPC and the MCP tools enforce too; a larger one is refused with 413
  `resource_exhausted`, naming the limit.
- **Changesets** travel as JSON (below). The server builds the changeset from it and hashes
  only its canonical encoding, so the id is the same as over gRPC; the JSON is never hashed.
  The repository is the one in the URL.
- **Deadline.** Every request gets at most 30 seconds, like an RPC.
- **Errors** answer with a status and
  `{"error": "<code>", "message": "<why>"}`, where `<code>` is the gRPC code's name:

  | HTTP | `error` |
  |---|---|
  | 400 | `invalid_argument` |
  | 401 | `unauthenticated`: no token where one is needed, a bad token, or a read of a repository that is not public without one |
  | 403 | `permission_denied`: the caller lacks the role, or the path is outside its directories |
  | 404 | `not_found` |
  | 409 | `already_exists`, `failed_precondition` (a lost compare-and-swap, not a fast-forward) |
  | 413 | `resource_exhausted`: a blob over the per-blob limit |
  | 503 | `unavailable` |
  | 504 | `deadline_exceeded` |
  | 500 | `internal`, `data_loss` |

### Changeset JSON

```json
{
  "id": "9f2c…",
  "parents": ["d41c…"],
  "author": {"name": "Ada", "email": "ada@example.com"},
  "author_date": {"unix_secs": 1791969120, "tz_offset_minutes": 180},
  "committer": {"name": "CI", "email": "ci@example.com"},
  "committer_date": {"unix_secs": 1791969130, "tz_offset_minutes": 0},
  "message": "Fix one",
  "file_changes": {
    "src/lib.rs": {"change": "tracked", "content": "<blob id>", "kind": "regular", "size": 27},
    "src/new.rs": {"change": "tracked", "content": "<blob id>", "kind": "executable", "size": 9,
                   "copy_from": {"path": "src/old.rs", "changeset": "<id>"}},
    "src/old.rs": {"change": "deleted"}
  },
  "extra": {"git-sha": "<hex bytes>"}
}
```

`id` appears in replies only. `committer`, `committer_date`, `copy_from` and `extra` may be
left out. `kind` is `regular`, `executable` or `symlink`. `file_changes` lists only the paths
that differ from the first parent, as in every changeset.

### Change request JSON

```json
{"id": 7, "head": "<id>", "base": "<id>", "status": "landed", "landed": "<id>",
 "conflicts": [], "submitted_unix_micros": 1791969120000000, "landed_unix_micros": 1791969120500000}
```

`status` is `pending`, `landing`, `landed`, `needs_rebase` or `rejected`. `landed` and
`landed_unix_micros` are present once it landed; `conflicts` lists paths when it needs a
rebase.

## Routes

All paths are under `/api/v1`. `{repo}` is a repository name. The role column is the role the
caller needs in that repository (`reader` < `writer` < `admin`).

### Repositories and refs

| Method and path | RPC | Role | Request | Reply |
|---|---|---|---|---|
| `GET /repos` | — | — | | `{"repos": [name]}`: where the caller holds a grant, and the public ones |
| `POST /repos` | `InitRepo` | server-wide `admin` | `{"repo": name}` | 201 `{"target": root, "version": 0}`; the caller becomes its admin |
| `GET /repos/{repo}/refs/{name}` | `GetRef` | `reader` | `name` is `main` or `release/<x>` | `{"target", "version", "root_tree"}` |
| `PUT /repos/{repo}/refs/{name}` | `MoveRef` | `admin` of the whole repository | `{"target", "expected_version"?}` | `{"version"}`; without `expected_version` the ref is created; fast-forward only |

### Objects

| Method and path | RPC | Role | Request | Reply |
|---|---|---|---|---|
| `GET /repos/{repo}/trees/{id}` | `GetTree` | `reader` | | `{"id", "entries": [{"name", "type": "tree", "id"} or {"name", "type": "blob", "id", "kind", "size"}]}`, by name |
| `GET /repos/{repo}/blobs/{id}` | `GetBlob` | `reader` | | the raw bytes |
| `POST /repos/{repo}/blobs` | `PutBlob` | `reader`, and a token | raw bytes | 201 `{"id"}` |

### Changesets and history

| Method and path | RPC | Role | Request | Reply |
|---|---|---|---|---|
| `POST /repos/{repo}/changesets` | `CreateChangeset` | `writer` on every changed path | changeset JSON; its blobs must be stored first | 201 `{"id", "root_tree"}` |
| `GET /repos/{repo}/changesets/{id}` | `Log` (limit 1) | `reader` | | changeset JSON with `"root_tree"`, the tree it leaves |
| `GET /repos/{repo}/log?head=&limit=&path=` | `Log`, or `PathLog` with `path` | `reader` (of `path`) | `limit` defaults to 50, at most 1000 | `{"changesets": [changeset JSON]}`, newest first along first parents; page by asking again from the last one's parent |
| `GET /repos/{repo}/diff?old=&new=` | `Diff` | `reader` | without `old`, against the empty tree | `{"entries": [{"path", "old": file or null, "new": file or null}]}` with file `{"id", "kind", "size"}`; only paths the caller reads |
| `GET /repos/{repo}/merge-base?a=&b=` | `MergeBase` | `reader` | | `{"base": id or null}` |
| `GET /repos/{repo}/blame?changeset=&path=` | `Blame` | `reader` of `path` | | `{"runs": [{"changeset", "lines"}]}` in file order |

### Rebase, merge and revert

Each replies `{"changeset", "root_tree", "created", "conflicted", "conflicts"}`: the resulting
changeset, or no `changeset` and the paths that stopped it in `conflicts`. `date` is
`{"unix_secs", "tz_offset_minutes"}` and defaults to now in UTC.

| Method and path | RPC | Role | Request |
|---|---|---|---|
| `POST /repos/{repo}/rebase` | `Rebase` | `writer` on the replayed paths | `{"head", "onto"}`; on a conflict `conflicted` names the changeset that could not be replayed |
| `POST /repos/{repo}/merge` | `Merge` | `writer` | `{"ours", "theirs", "author": {"name", "email"}, "date"?, "message"}` |
| `POST /repos/{repo}/revert` | `Revert` | `writer` | `{"head", "changeset", "author", "date"?, "message"}`; `conflicts` lists the paths changed since |

### Change requests

| Method and path | RPC | Role | Request | Reply |
|---|---|---|---|---|
| `POST /repos/{repo}/change-requests` | `SubmitChangeRequest` | `writer` | `{"head", "wait"?}`; with `"wait": true` the reply comes once the request is finished, or after at most 20 seconds | 201 change request JSON |
| `GET /repos/{repo}/change-requests?status=&limit=` | `ListChangeRequests` | `reader` | `limit` defaults to 100, at most 1000 | `{"requests": [change request JSON]}`, newest first |
| `GET /repos/{repo}/change-requests/{id}?wait=` | `GetChangeRequest` | `reader` | `wait=true` as for submitting | change request JSON |
| `GET /repos/{repo}/landings?changesets=a,b` | `FindLanding` | `reader` | up to 1000 comma-separated ids | `{"landings": [{"changeset", "request"}]}`, in the order asked, leaving out changesets no request landed |

### Access

| Method and path | RPC | Role | Request | Reply |
|---|---|---|---|---|
| `GET /repos/{repo}/grants` | `ListGrants` | `admin` of the whole repository | | `{"grants": [{"principal", "path", "role"}]}` |
| `PUT /repos/{repo}/grants` | `Grant` | `admin` of the whole repository | `{"principal", "role", "path"?}`; `anyone` may only be `reader` | 204 |
| `DELETE /repos/{repo}/grants?principal=&path=` | `Revoke` | `admin` of the whole repository | | 204, or 404 if there was no such grant |
| `GET /whoami?repo=` | `WhoAmI` | a token | | `{"principal", "expires_unix_seconds", "grants"}` |
| `GET /version` | — | — | | `{"version"}`: the server's version, as `scion-server --version` prints it |

## From init to land

```sh
api=http://127.0.0.1:8080/api/v1 auth="Authorization: Bearer $SCN_TOKEN"
curl -sH "$auth" -d '{"repo":"demo"}' $api/repos                  # {"target":"<root>",...}
blob=$(curl -sH "$auth" --data-binary @hello.txt $api/repos/demo/blobs | jq -r .id)
curl -sH "$auth" -H 'content-type: application/json' $api/repos/demo/changesets -d '{
  "parents": ["<root>"], "author": {"name": "Ada", "email": "ada@example.com"},
  "author_date": {"unix_secs": 1791969120, "tz_offset_minutes": 0}, "message": "add hello",
  "file_changes": {"hello.txt": {"change": "tracked", "content": "'$blob'",
                                 "kind": "regular", "size": 6}}}'   # {"id":"<head>",...}
curl -sH "$auth" -H 'content-type: application/json' $api/repos/demo/change-requests \
  -d '{"head": "<head>", "wait": true}'                             # "status":"landed"
curl -sH "$auth" $api/repos/demo/refs/main                          # main is <head>
```

A clone reads `refs/main`, then `trees/{root_tree}` and each tree below it, then the blobs it
wants; nothing else is needed. `crates/scion-server/tests/api.rs` runs this loop, and every
route above, against the server.
