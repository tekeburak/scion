# Competitive notes

Other open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/research/redesigning-source-control).
This file records only how they **present** themselves, to learn presentation technique.
Scion takes no code, design decision or architecture from them, and copies no text or image
from any of them (several are GPL-licensed). None of these names or links belong in the
README.

## 2026-11-27

Same method as 2026-11-26 (repository search for stars, forks and last push; anonymous shallow
clones under the scratchpad for commits and tags). "Last 24 h" means commits dated 2026-11-26
06:00 UTC or later. This branch is stacked on the 2026-11-26 notes, which are not on `main` yet.

| Repository | Stars / forks (11-26 → today) | Last push | Last 24 h | Noted |
|---|---|---|---|---|
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 976 / 100 → 1,014 / 101 | 2026-11-22 | none | Passed 1,000 stars; no new commit. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 321 / 27 → 323 / 29 | 2026-10-16 | none | Quiet; no tag. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 163 / 24 → 165 / 24 | 2026-10-27 | none | Quiet; no tag. |
| [bnsd55/openscion](https://github.com/bnsd55/openscion) | 30 / 5 → 30 / 5 | 2026-11-26 | 3 commits | A reopen right after a drop or refused open waits for the store lock (test); two dependency-bump PRs. Still at `v0.1.0`. |
| [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) | 0 / 0 | 2026-10-18 | none | Quiet. |
| [RKKatakam/veil](https://github.com/RKKatakam/veil) | 30 / 2 → 30 / 2 | 2026-11-24 | none | Quiet since its README rewrite; about 2,000 commits since 2026-10-26. README benchmark claims unchanged (207 batched landings/s on one `main` across a cloud cluster; exactly-once landing through a server failover; 3.1× faster default clone than git on a 1 GB project). |
| [yuchenZhangTG/graft](https://github.com/yuchenZhangTG/graft) | 24 / 0 | 2026-11-05 | none | Quiet. |
| [baz-scm/gion](https://github.com/baz-scm/gion) | 32 / 4 | 2026-11-16 | none | Latest tag `v0.7.0`. |

New repositories created after 2026-10-13 matching the searches: none reached 5 stars
(`stigenai/scion`, 0 stars, was pushed today).

### What it means for Scion

- No new direction since yesterday. Each claim above already has an open idea issue: sustained
  land throughput (#317), exactly-once landing through a server failover (#306), newest green
  `main` (#319), bisect (#274). No issue filed this run.
- **Our CI is red for reasons outside the diff**, so the daily notes PRs (2026-10-27 onward) stay
  unmerged: on #320 the `fmt + clippy + test` job died with a linker bus error (signal 7) on the
  self-hosted runner, and `cargo-deny` failed on advisories. Both need the owner.

## 2026-11-26

First reading since 2026-10-26. Repository search answered (stars, forks, last push and license
below are from it); per-repository API calls were not used, so commits and tags come from
anonymous shallow clones under the scratchpad and features from each README as the project
states it. "Last 24 h" means commits dated 2026-11-25 or later.

| Repository | Stars / forks (10-26 → today) | Last push | Last 24 h | Noted |
|---|---|---|---|---|
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 988 / 97 → 976 / 100 | 2026-11-22 | none | Still the most-starred; stars slightly down. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 245 / 19 → 321 / 27 | 2026-10-16 (as search reports it) | none | Stars keep growing without new commits; last default-branch commits are mid-October (benchmarks, daemon hit rate, a landing soak). No tag. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 119 / 18 → 163 / 24 | 2026-10-27 | none | Quiet; README reframed around its benchmark chart against git. GPL-3.0: nothing copied. No tag. |
| [bnsd55/openscion](https://github.com/bnsd55/openscion) | 30 / 3 → 30 / 5 | 2026-11-25 | 8 commits | **Tag `v0.1.0`** (tagged commit dated 2026-11-01, "the publish path runs") and ~450 commits since 10-27. README now links a "verify a release" page: signed tag, checksums and attestations. Last day's subjects: GOAWAY at a connection age with the client resending calls the server never processed; land-model test failures carry the server state; per-RPC metrics without per-RPC strings; nextest in CI. |
| [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) | 0 / 0 → 0 / 0 | 2026-10-18 | none | Quiet. |
| `rizaust/open-scion`, `adrianchifor/scn`, `yumaitau/yuma-agent-git` | 3→4, 1→2, 0 | 10-14, 10-17, 11-09 | none | Quiet. |
| `OpenCSGs/scion`, `hermes98761234/sgit` | not read | – | – | Missing from search results today. |
| **new:** [RKKatakam/veil](https://github.com/RKKatakam/veil) | 30 / 2 | 2026-11-24 | none | Rust, Apache-2.0, created 2026-11-03. A jj-style client on a central server. README states: per-folder permissions on every read, batched landings in one compare-and-swap, **land first, test after: agents start from the newest green `main`, and the server auto-bisects a failure and reverts the culprit**, client-side encryption of secret-looking files, a passkey web UI, and a multi-machine cloud benchmark dated 2026-11-20/21 with an "honest misses" list (e.g. 393 branch pushes/s). Tag `pre-git-removal` only. |
| **new:** [yuchenZhangTG/graft](https://github.com/yuchenZhangTG/graft) | 24 / 0 | 2026-11-05 | none | Apache-2.0, created 2026-11-02. Virtualized workspaces: FUSE lower layer with on-demand chunk fetch plus a native copy-on-write upper layer; claims a writable workspace at any commit in < 50 ms regardless of repository size; a cloud linearizable store for branch CAS and agent leases. No tag. |
| **new:** [baz-scm/gion](https://github.com/baz-scm/gion) | 32 / 4 | 2026-11-16 | none | Go, created 2026-10-15. Not a server: a git client for agents that fetches only what is needed from git forges; releases published; first-screen chart claims a full-tree grep of a 510k-file repository in 3.49 s vs 203 s for git. |

Also seen (0–3 stars, not tracked): about fifteen more Scion-inspired repositories created
since 2026-10-14, among them `utopia-ia/elffuss-braid` (3), `garyships/kudzu` (2), and several
one-commit clones and explainers.

### What it means for Scion

- **Land first, test after.** The post's own example of a tool built on the source-control API
  (automated bisect and revert after a post-merge failure) is now stated as built by one project,
  together with "start from the newest green `main`". Scion has `scn bisect` as an open idea
  (#274) and a server-side revert (5.10), but no record of which landed changeset passed CI, so
  an agent cannot ask for "the newest green `main`". This run's issue proposes that first step.
- **Releases.** Two more tracked projects now have a tag; one documents release verification
  (signed tag, checksums, attestations). Scion's Phase 9 (9.2 binaries, #281 provenance) covers
  it; no new issue.
- **Virtualized workspaces** with a sub-50 ms claim: Phase 8 (planned) and #295 (`scn mount`)
  already cover the first step; no new issue.
- **Benchmarks with misses listed.** One new README publishes its misses next to its wins;
  Scion's `docs/BENCHMARKS.md` already keeps a misses list.
- **Graceful connection handover** (GOAWAY plus client resend of unprocessed calls) is #179's
  territory; no new issue.

## Older readings (more than 30 days, one line each)

- **2026-10-26:** three new small projects (Go client over git, an edge-platform TypeScript one, a docs-only one); two projects tagged first releases, one added a git gateway (non-goal); weekly table: Scion ahead on web UI + HTTP API + MCP + GitHub sign-in + ACLs, behind on daemon tier and release (#265 filed).
- **2026-10-25:** search answered, nothing new; one project vanished from search under its old spelling.
- **2026-10-24:** stars back from repository pages; nine projects new to the notes listed.
- **2026-10-23:** stars missing (API and pages refused); commits from anonymous clones only; a second run the same day.
- **2026-10-22:** stars missing; anonymous clones only.
- **2026-10-21:** token refused; `competitive-stars.csv` started, backfilled.
- **2026-10-19:** first-screen presentation table by repository; new projects since 10-18 marked.
- **2026-10-18:** first reading: presentation techniques (topics, description, benchmarks near the top, "what it is not" early).
