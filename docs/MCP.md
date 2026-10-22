# MCP server

`scion-mcp` gives coding agents Scion as [Model Context Protocol](https://modelcontextprotocol.io)
tools and resources. It is a client of the server's HTTP/JSON API (`docs/API.md`): every tool
call becomes one or a few API requests made with one bearer token, so roles, directory grants,
validation and the land worker are the server's own, and `scion-mcp` holds no state.

## Running it

```sh
# the server (gRPC on SCION_LISTEN, default 0.0.0.0:7420), with its web address
# for the pages and /api/v1
scion-server --web 127.0.0.1:8080

# a token: a person's from `scn login`, or a service principal's
export SCN_TOKEN=$(cat ~/.config/scn/tokens/http___127.0.0.1_7420)
export SCN_TOKEN=$(scion-server token sp:agent --ttl 30d)

# stdio, for an agent that starts the server itself (the default)
scion-mcp --api http://127.0.0.1:8080/api/v1

# streamable HTTP at http://127.0.0.1:7421/mcp, for agents that connect by URL
scion-mcp --api http://127.0.0.1:8080/api/v1 --http 127.0.0.1:7421
```

`--api` may also come from `SCION_API`. Without `SCN_TOKEN` only public repositories read and
nothing writes. A file over the server's per-blob limit (64 MiB) is refused by
`write_files_and_commit`, naming the limit, before anything is uploaded. The HTTP transport accepts loopback `Host` headers only, so it serves the
machine it runs on; every caller shares the one token.

An agent that takes a command, Claude Code for one:

```sh
claude mcp add scion --env SCN_TOKEN=$SCN_TOKEN -- scion-mcp --api http://127.0.0.1:8080/api/v1
```

The MCP inspector lists the tools and calls them by hand:

```sh
npx @modelcontextprotocol/inspector scion-mcp --api http://127.0.0.1:8080/api/v1
```

## Tools

A revision is `main`, `release/<x>` or a 64-hex changeset id; it defaults to `main`. Paths are
repository paths (`/`-separated, no leading `/`). Every tool answers with JSON text, except
`read_file`, which answers with the file's text. A refusal is a tool error whose text is the
server's `<code>: <message>` (`permission_denied: …`, `not_found: …`), so the agent sees why.

| Tool | Arguments | Does |
|---|---|---|
| `list_repos` | | the repositories the token reads |
| `tree` | `repo`, `path`?, `at`? | a directory's entries: name, `tree` or `blob`, kind and size |
| `read_file` | `repo`, `path`, `at`? | the file's text; a binary file is refused with its size |
| `log` | `repo`, `head`?, `path`?, `limit`? | changesets newest first along first parents, only those changing `path` if given |
| `blame` | `repo`, `path`, `at`? | runs of lines with the changeset that last changed each |
| `diff` | `repo`, `new`, `old`? | the paths that differ, with old and new blob ids; `old` defaults to `new`'s first parent |
| `changeset` | `repo`, `id` | one changeset: parents, author, message, the files it changes |
| `write_files_and_commit` | `repo`, `parent`?, `message`, `author_name`, `author_email`, `files` | one new changeset on `parent`; each file is `{path, content, executable?}`, and leaving out `content` deletes the path. Nothing moves |
| `land` | `repo`, `head` | submits `head` as a change request and waits up to 20 s: `landed` (with the landed changeset), `needs_rebase` (with the conflicting paths) or still `pending` |
| `change_request` | `repo`, `id`?, `status`? | one change request, or the newest ones |

Every tool but `write_files_and_commit` and `land` is marked read-only.

The loop an agent runs: `tree` and `read_file` to look, `write_files_and_commit` to change
files, `land` to put the change on `main`. When `land` answers `needs_rebase`, read the
conflicting paths at `main`, commit the resolution on `main` and land that.

## Resources

One template, `scion://{repo}/{+path}@{revision}`: a file's text, or a directory's entries as
JSON, at a revision, e.g. `scion://demo/src/lib.rs@main`, `scion://demo/@release/1` for the
top directory, or a path at a changeset id.

## Cost

`tree`, `read_file` and a resource read the revision (one request), then one tree per path
component, then the blob. `write_files_and_commit` uploads one blob per written file and
creates one changeset. `land` is one request that the server holds while the land worker
runs. `crates/scion-mcp/tests/tools.rs` lists the tools as an inspector does and runs a commit →
land loop through them on stdio and on streamable HTTP.
