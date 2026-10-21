# Data model

This document is normative for everything that is hashed. The executable form of this
specification is the golden fixture set in `crates/scion-types/tests/golden/`; if the two ever
disagree, that is a bug in one of them and must be fixed before anything else.

## Identifiers

All ids are 32-byte BLAKE3 digests, rendered as 64 lowercase hex characters.

| Id | Definition |
|---|---|
| `BlobId` | `blake3(raw file bytes)` |
| `TreeId` | `blake3(canonical(tree))` |
| `ChangesetId` | `blake3(canonical(changeset))` |

Identical content produces identical ids everywhere, which is what makes every object cacheable
by any tier without coordination.

## Paths

A `RepoPath` is a UTF-8 string of one or more components separated by `/`. Rules:

- non-empty; no leading or trailing `/`; no empty component (`//`);
- no component may be `.` or `..`; no NUL bytes;
- **byte-exact**: no Unicode normalisation, no case folding;
- ordering is byte-lexicographic over the whole string. This ordering is what the canonical
  encoding uses for maps.

A `PathComponent` is a single component under the same rules, and additionally may not contain
`/`.

## Canonical encoding

The canonical encoding is the *only* input to a hash. It is not protobuf and never will be:
protobuf permits unknown fields, unordered map entries and several varint spellings of the same
value, none of which may influence an id.

### Primitives

All integers are big-endian.

| name | encoding |
|---|---|
| `u8` | 1 byte |
| `u16` | 2 bytes |
| `i32` | 4 bytes, two's complement |
| `u64` | 8 bytes |
| `i64` | 8 bytes, two's complement |
| `id` | 32 raw digest bytes |
| `bytes` | `u64` length, then that many raw bytes |
| `str` | `bytes` of the UTF-8 encoding; decoders reject invalid UTF-8 |
| `opt(T)` | `u8` `0x00` = absent, or `0x01` followed by `T` |
| `seq(T)` | `u64` count, then `count` × `T` |
| `map(K,V)` | `seq` of `(K, V)` pairs in **strictly increasing** byte order of `K` |

Every object begins with a 4-byte magic and a `u16` format version. The current version is
**1** for both object kinds.

Decoders are strict: they reject a wrong magic, an unknown version, an unknown tag byte, an
invalid path, unsorted or duplicate map keys, truncated input, and trailing bytes. Consequently
each value has exactly one valid encoding.

### Changeset (`SCCS`, version 1)

<p align="center">
  <img src="assets/changeset.svg" width="800" alt="A changeset's fields, its file changes pointing at blobs, and the derived data computed from it">
</p>

The figure is illustrative (the example paths are made up); the layout below is normative.

```
magic           4 bytes   "SCCS"
version         u16       1
repo            str       repository name (part of the hash)
parents         seq(id)   first parent is the mainline
author          str name, str email
author_date     i64 unix seconds, i32 UTC offset in minutes
committer       opt(str name, str email)
committer_date  opt(i64, i32)
message         str
file_changes    map(str path, file_change)
extra           map(str key, bytes value)
```

`file_change`:

```
tag u8 = 0x00   Deleted
tag u8 = 0x01   Tracked:
                  content    id      BlobId
                  kind       u8      0 regular, 1 executable, 2 symlink
                  size       u64
                  copy_from  opt(str path, id changeset)
```

Notes:

- `file_changes` lists only paths that differ from the **first parent** (or from the empty tree
  for a root changeset). For a merge, changes relative to other parents are not recorded; history
  and blame follow the first-parent line, which is the only line that matters in trunk-based
  development. This is a stylistic choice inherited from the reference design.
- There is **no tree pointer**. Trees are derived.
- There is **no signature field**. Signatures are detached: stored beside the changeset (the
  `signature` column) and computed over the `ChangesetId`, so signing does not change the id.
- `extra` is for provenance such as `git-sha`; it *is* part of the hash.

### Tree (`SCTR`, version 1)

```
magic     4 bytes   "SCTR"
version   u16       1
entries   map(str component, tree_entry)
```

`tree_entry`:

```
tag u8 = 0x00   Tree:  id TreeId
tag u8 = 0x01   Blob:  id BlobId, kind u8 (as above), size u64
```

The empty tree encodes as `53 43 54 52 00 01 00 00 00 00 00 00 00 00` and has id
`blake3` of those 14 bytes (see `tests/golden/empty_tree.hash`).

### Worked example

`tests/golden/full_changeset.bin` is the canonical encoding of a changeset mirroring the
"shape of a changeset" figure from the reference article: two parents, a separate committer, one
modified file with copy provenance, one deleted file, one executable, and a `git-sha` extra.
Its hash is in `full_changeset.hash`. Decode it by hand against the table above when reviewing
any change to the encoder.

### Changing the format

Bump `FORMAT_VERSION`, keep the old decoder for reading existing objects, regenerate the golden
fixtures with `UPDATE_GOLDEN=1 cargo test -p scion-types --test golden`, and add a dated
subsection here describing the change. A version bump changes every id, so it is a migration,
not a patch.

## Where things live

| Data | Location | Key |
|---|---|---|
| Changeset metadata + `file_changes` | metadata DB (`changesets`, `file_changes`) | `(repo, id)` |
| Blob bytes | object store | `blobs/{id}` |
| Trees | metadata DB (`trees`) from Phase 10.3; object store `trees/{id}` until then | `(repo, id)` |
| Root tree of a changeset | metadata DB (`derived`, type `root_tree`) | `(repo, "root_tree", v, changeset)` → `TreeId` |
| Path-history pages and other derived data | metadata DB (`pages`) from Phase 10.3, pointer in `derived`; object store until then | per type |
| Refs | metadata DB (`refs`) | `(repo, name)` |
| Snapshot manifests (Phase 4, planned) | object store | `snapshots/{repo}/{changeset}.manifest` |

Everything in the object store and in the `trees` and `pages` tables is immutable, addressed
by hash and append-only; objects in the store are zstd-compressed. Garbage collection is a
non-goal for now.

### Deviation: trees in the object store

The reference design keeps trees in the metadata database (Spanner) next to changesets and
refs. Scion first kept them in the object store, so that a tree could ride the same
daemon → cache → server path as a blob and the metadata database stayed small. The real
benchmark (Phase 6.9–6.13) showed the cost: a commit writes one object per rewritten tree and
history page, an import pays one object-store round trip per tree, and a cold `log` or `blame`
pays one per pointer hop. Phase 10.3 therefore moves trees and history pages to the metadata
database, as the reference design has them: they are still immutable and content-addressed
(the canonical encoding and the golden fixtures do not change), the tiers still cache them by
id, and blobs stay one immutable object each in the object store.

## Repositories

A repository is a name on the server (the `repos` table; `repo` in every other table and in
every changeset). It exists once its **root changeset** has been stored: the changeset
`Changeset::repository_root(repo)` returns, with no parents, author `scion` with an empty email,
author date `0 +0000`, no committer, message `Create repository`, no file changes and no
`extra`. It depends only on the name, so every client builds the same one. `scn init <server>
<repo>` calls `InitRepo`, the only RPC that creates a repository (it needs a server-wide
`admin` grant): the server stores the root changeset, creates `main` at it and makes the caller
the repository's `admin`. `CreateChangeset` refuses a root changeset. Every other write naming a
repository that does not exist fails, from a client with `PERMISSION_DENIED` (nobody holds a
role there) and from `scion-ingest` with `NOT_FOUND`, so a misspelled name never starts a new
repository.

## Refs and anonymous heads

Only two kinds of ref exist on the server: `main` and `release/<name>`. Creating or moving them
will require admin rights once IAM lands (Phase 7); until then the server has no
authentication. A ref is a single row `(repo, name, target, version)`; moving it is
`UPDATE … WHERE version = $observed`, a compare-and-swap. There is no other global mutable
state.

Everything else is an **anonymous head**: a changeset id the client remembers. There is no
`branch` command. The working copy's `.scn/state` file holds:

```toml
current_head = "9f2c…"          # changeset the working copy is based on
base          = "d41c…"          # last known merge-base with main (a cache)

[heads]                          # local labels for changesets this client cares about
"fix-cache-tier" = { head = "9f2c…", updated = "2026-10-14T09:12:00Z" }
```

- `scn commit` creates the changeset on the server (it is immediately addressable by anyone who
  knows the id) and advances `current_head`. No server ref moves.
- Labels never leave the machine. `scn log`, `diff`, `blame`, `checkout` and `update` accept a
  label wherever they take a changeset; `scn heads` (list labels) is planned and not built yet,
  so no command creates a label today.
- `scn checkout <rev>` moves the working copy to any changeset and `current_head` with it.
  `scn update [rev]` (default `main`) has the server replay `current_head`'s changesets that
  `rev` does not contain on top of it, then moves the working copy to the result and sets
  `base` to `rev`. Both touch only the files that differ, and both refuse local edits.
- `scn update --conflicts [rev]` does not refuse a conflict: it moves `current_head` and `base`
  to `rev` and leaves every change the old head made since its merge base with `rev` as local
  edits, merged per file, with three-way conflict markers where both changed the same lines. The
  conflicting paths are listed one per line in `.scn/conflicts`; `scn commit` refuses while any
  of them still has a conflict block, then creates the resolved changeset on `rev` (the old
  head's changesets land as that one) and empties the list.
- Because the object store is append-only and there is no GC, an anonymous head cannot be lost
  by forgetting it; it can always be re-attached by id.
- Sharing work means sharing an id, or landing it: `scn land` submits a change request, the
  server rebases the head onto `main` and moves `main`, and the working copy follows the
  landed changeset.

## Generation numbers

Every changeset row stores `generation = max(parent.generation) + 1` (roots are 0). Together with
the phase-2 skip-list index this bounds ancestry and merge-base queries without walking the whole
graph.
