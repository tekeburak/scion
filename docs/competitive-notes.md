# Competitive notes

Other open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/research/redesigning-source-control).
This file records only how they **present** themselves, to learn presentation technique.
Scion takes no code, design decision or architecture from them, and copies no text or image
from any of them (several are GPL-licensed). None of these names or links belong in the
README.

## 2026-11-23

Repository search answered; per-repository API calls stayed refused (the session's `gh` token
is invalid for anything but search), so commits come from anonymous partial clones under the
scratchpad (subjects only), tags from `git ls-remote`, features from each README as the
project states it. Window for "last 24 h": since 2026-11-22 06:20 UTC. The readings of
2026-11-09 to 2026-11-22 sit in open notes PRs (#293–#313) that CI has not let merge; the
"since" column compares with the 2026-11-22 reading recorded in #313.

| Repository | Stars / forks (11-22 → today) | Last push | Last 24 h | Noted |
|---|---|---|---|---|
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 1,004 → 1,005 / 101 | 2026-11-22 03:25 | none (the last push, a NOTICE/README branch, is before the window) | Tags end at `v0.4.2` (2026-11-05). Benchmark claim unchanged. |
| [bnsd55/openscion](https://github.com/bnsd55/openscion) | 30 / 5 | 2026-11-21 04:08 | none | ~450 commits since 2026-10-26, tests, audits and soaks (a Postgres fault soak now nightly). Only tag `v0.1.0`. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 317 → 319 / 26 | 2026-10-16 | none | Quiet five weeks; no tags. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 158 → 159 / 23 → 24 | 2026-10-27 | none | Quiet; no tags. GPL-3.0: nothing read beyond the description. |
| [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) | 0 / 0 | 2026-10-18 | none | Dormant. |
| [yuchenZhangTG/graft](https://github.com/yuchenZhangTG/graft) | 23 / 0 | 2026-11-05 | none | No tags. README claims "< 50 ms" isolated workspaces (FUSE lower layer, native copy-on-write upper layer) and a syntax-tree merge; no measured data published. |
| `TheDancingDeveloper-org/graft` | 0 / 0 | 2026-11-21 01:29 | none | Tags `v0.1.0`, `v0.1.1`; recent work is CI and a mirror. AGPL-3.0: nothing copied. |

Search (created after 2026-10-13; "scion" with source control / monorepo / anthropic, and
"redesigning source control"): no new project on the topic with ≥ 5 stars. The
`Scion-Project/*` results (10 and 7 stars) are phone firmware, unrelated. `OpenCSGs/scion`
stays gone from search. Scion itself: 0 stars, 0 forks.

### What it means for Scion

- **Nothing moved in the last 24 hours**: no commit on a watched default branch, no tag, no
  new benchmark claim.
- **Workspace and merge claims** of the newest tracked project map to existing ideas: a lazy
  read-only mount (#295, Phase 8) and smarter merges for insert/insert conflicts (#292). No
  new issue.
- **CI still blocks the notes** and every other PR: `cargo-deny` is red on `main` on advisories
  against locked dependencies, and the dependency PR #305 is red too. This PR will stay open
  like #293–#313 until that is fixed on `main`.

### Weekly: ahead / behind (2026-11-23)

From each README and docs as stated today; ✓ stated, – absent or stated as not done, ? not
stated. Scion's column is from its own README and `docs/ROADMAP.md` on `main` (unchanged since
2026-10-26). `OpenCSGs` is dropped (gone); `graft (Y)` is `yuchenZhangTG/graft`,
`graft (T)` is `TheDancingDeveloper-org/graft`.

| Feature | Scion | CarterT27 | tinyhumansai | bnsd55 | kianaaa19 | graft (T) | graft (Y) | feder-cr |
|---|---|---|---|---|---|---|---|---|
| CLI | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Server-side land | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ server-side merge | ✓ |
| Persistent store | ✓ Postgres + S3 | ✓ CockroachDB + S3 | ✓ SQLite, files | ✓ SQLite + files (Postgres exp.) | ✓ SQLite | ? | ✓ SQLite + S3 (Spanner stated) | ✓ SQLite or Postgres + object store |
| Derived data | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ? | ✓ |
| Daemon / cache tier | – (Phase 4) | ✓ | ✓ | ✓ (+ read-only mount, exp.) | – | ✓ node | ✓ daemon + FUSE | ✓ |
| Git import | ✓ | ✓ | ? | ✓ | ? | ✓ | ? | ✓ shadow mode |
| Git gateway | – non-goal | – | – | – | – | – export only | – | ? |
| Auth | ✓ GitHub, SPs, ACLs | ✓ tokens | ? | ✓ tokens, tickets | ✓ passwords, tokens | ✓ tokens | ? | ✓ per-agent tokens |
| Web UI | ✓ | ? | ✓ | ? | ✓ | ? | ✓ | ? |
| HTTP API | ✓ | ? | ✓ | ? | ✓ | ? | ? | ? |
| MCP | ✓ | ? | ? | ? | – | ✓ (+ LSP) | ? | ? |
| Benchmark | ✓ measured | ✓ | ✓ | ✓ | – | ✓ | claim only | ✓ |
| Release | – (Phase 9) | – | – | ✓ v0.1.0 | – | ✓ v0.1.1 | – | ✓ v0.4.2 |

**Ahead:** still the only project stating web UI, HTTP API and MCP together, with GitHub
sign-in and directory ACLs on a Postgres + S3 store. **Behind:** no daemon/cache tier (six of
seven others state one) and no tagged release (three ship one). Scion's `main` has not moved
in four weeks because CI is red on dependency advisories, which is now the largest gap: every
other gap needs a merged PR.

## 2026-10-26

The GitHub API answered this session again (repository search; READMEs of repositories outside
this one stayed refused, so the three new projects below were read from anonymous shallow
clones under the scratchpad, README first screen only). A search for repositories about Scion
or source control, sorted by last update, found three projects these notes had not listed.
Stars are from the search API today; `competitive-stars.csv` gets today's rows.

| Repository | Stars (2026-10-24 → today) | Noted |
|---|---|---|
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 980 → 987 | Still the most-starred; topics now include `scion`. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 237 → 245 | Pushed on 2026-10-25. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 113 → 119 | Pushed on 2026-10-25; 13 open issues. |
| [OpenCSGs/scion](https://github.com/OpenCSGs/scion), [rizaust/open-scion](https://github.com/rizaust/open-scion), [adrianchifor/scn](https://github.com/adrianchifor/scn), `kianaaa19/scionhub` | 3, 3, 1, 0 (unchanged) | Only `OpenCSGs/scion` was pushed since the last run. |
| `TheDancingDeveloper-org/graft` | 0 | Pushed today; AGPL-3.0, so nothing is copied from it. |
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) | not read | Missing from search results today although `git ls-remote` still reaches it. |
| **new:** [hermes98761234/sgit](https://github.com/hermes98761234/sgit) | 0 | Go, MIT, created 2026-10-14. Scion-style client and server layered over git on the server. First screen: one sentence, the link to Anthropic's post, then a **Features** list with a bold lead word per bullet. Eight topics, `scion-style` among them. |
| **new:** [yumaitau/yuma-agent-git](https://github.com/yumaitau/yuma-agent-git) | 0 | TypeScript on one hosted edge platform, created 2026-10-14, 72 open issues. First screen: a bold one-line tagline, the inspiration sentence with the post's link, a **Why** section, then numbered "core principles" restating the post. |
| **new:** `MeroZemory/scion-clone` | 0 | A `docs/` directory only; no code yet. |

Also seen, not a project: a Chinese-language explainer repository about the post (no code).

### What it means for Scion

- **Bold lead words in the feature list.** One new project opens every feature bullet with a
  bolded term, the same technique Scion's "What works today" list already uses; no change.
- **A "Why" section right after the tagline** is on both new READMEs' first screen, as on
  Scion's ("Why Scion"); the order the rules set already matches.
- **Topics.** Two projects carry `scion` as a topic; Scion's topics (`source-control`,
  `monorepo`, `coding-agents`, …) do not, and adding a topic is a repository setting, not a file;
  left to the owner, noted only.

### 2026-10-26, second run (03:30 UTC)

Repository search answered; per-repository API calls stayed refused, so commits and tags come
from anonymous shallow clones under the scratchpad (subjects only) and features from each
README as the project states them. Window: since the 2026-10-25 later reading.

| Repository | Stars / forks | Since the last reading | Noted |
|---|---|---|---|
| [bnsd55/openscion](https://github.com/bnsd55/openscion) | 30 / 3 | 33 commits, no tag | Back in search under this name (the earlier `open-scion` spelling). Tests and docs hardening again: a cache refuses tickets no key signed, an empty auth token refused everywhere, a `gc --dry-run`, archive restore in bounded transactions, every measured number names its run and commit, the writable mount leaves the default build. |
| [OpenCSGs/scion](https://github.com/OpenCSGs/scion) | 3 / 0 | 2 commits, **tag `v0.1.11`** | Release 0.1.11 is **a git gateway** (`git clone`, `fetch`, `push` against the server). README already offers an install script (systemd/launchd), a one-container Docker image on SQLite + local directory, and a Postgres + S3 production image. |
| `TheDancingDeveloper-org/graft` | 0 | ~30 commits, **tag `v0.1.0`** (first release) | `graft serve --dev`, a zero-config dev server that prints the clone command; a local in-process mode; clone by URL; named servers with `graft login`; one binary for serve/node/mirror; one-line MCP registration for three agent clients; a published bench chart. AGPL-3.0: nothing copied. |
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 988 / 97 | none (tags up to `v0.4.0`) | Quiet since 2026-10-23. |
| [CarterT27/scion](https://github.com/CarterT27/scion), [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion), [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) | 245, 119, 0 | none | Quiet; no tags. |

No new repository created after 2026-10-13 reached 5 stars (the largest newcomer has 2).

**Direction.** Packaging is where the field moved this week: two projects tagged releases with
binaries and images, and three offer a first try with no external services (a dev server, one
container on a single-file store, or an install script). One added a git gateway, which stays a
Scion non-goal (git wire protocol). Issue #265 proposes the one gap worth a step: a dev server
on the memory stores so a first try needs no Docker.

**The post itself.** The post now lives at `/research/redesigning-source-control` (the old
`/engineering/` address answers with a permanent redirect; the link above follows it). Its note
"Updated October 15" adds credit to Sapling/Mononoke for techniques Scion uses; Scion's README
already lists them under "Prior art". Two points of the post that the table below does not
cover, recorded for later runs: tools that run on the source-control API alone, its example
being automated bisect and revert after a post-merge test failure (one project above states a
test-after-land, bisect-and-revert tool), and the statement that the system is not offered
publicly, so every project here, Scion included, is an independent reading of it.

### Weekly: ahead / behind (2026-10-26)

From each README and docs as stated today; ✓ stated, – absent or stated as not done, ? not
stated. Scion's column is from its own README and `docs/ROADMAP.md`.

| Feature | Scion | CarterT27 | tinyhumansai | bnsd55 | kianaaa19 | OpenCSGs | graft | feder-cr |
|---|---|---|---|---|---|---|---|---|
| CLI | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Server-side land | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Persistent store | ✓ Postgres + S3 | ✓ CockroachDB + S3 | ✓ SQLite, files (memory reference) | ✓ SQLite + files (Postgres exp.) | ✓ SQLite | ✓ SQLite or Postgres + S3 | ? | ✓ SQLite or Postgres + object store |
| Derived data | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ✓ |
| Daemon / cache tier | – (Phase 4) | ✓ | ✓ | ✓ | – | ✓ | ✓ node | ✓ |
| Git import | ✓ | ✓ | ? | ✓ | ? | ? | ✓ | ✓ shadow mode |
| Git gateway | – non-goal | – | – | – | – | ✓ v0.1.11 | – export only | ? |
| Auth | ✓ GitHub, SPs, ACLs | ✓ tokens | ? | ✓ tokens, tickets | ✓ passwords, tokens | ✓ tokens, OIDC/GitHub | ✓ tokens | ✓ per-agent tokens |
| Web UI | ✓ | ? | ✓ | ? | ✓ | ? | ? | ? |
| HTTP API | ✓ | ? | ✓ | ? | ✓ | ? | ? | ? |
| MCP | ✓ | ? | ? | ? | – | ? | ✓ (+ LSP) | ? |
| Benchmark | ✓ | ✓ | ✓ | ✓ | – | ✓ | ✓ | ✓ |
| Release | – (Phase 9) | – | – workflow only | – | – | ✓ v0.1.11, image | ✓ v0.1.0 | ✓ v0.4.0 |

**Ahead:** the full agent surface (web UI, HTTP API and MCP together) with GitHub sign-in and
directory ACLs on a Postgres + S3 store; no other project states all three plus that store.
**Behind:** no daemon/cache tier yet (Phase 4, planned; five projects state one), no tagged
release or image (Phase 9 in progress; three projects ship one), and the heaviest first try
(compose stack) — #265.

## 2026-10-25

The GitHub API refused this session (search and every repository outside this one), so no
star counts, no topics and no search for new projects; `competitive-stars.csv` gets no row
today. Commits come from anonymous shallow clones under the scratchpad, read as commit
subjects only, for everything since the later reading of 2026-10-24.

| Repository | Since the last run | Noted |
|---|---|---|
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) | 13 commits | Mostly tests and CI hygiene (every action at one version, service containers pinned by digest), plus documentation passes that close gaps an operator walk-through found, and a "published envelope" of capacity numbers tied to one CI run id. Its "Try it" README section was corrected to walk the repository it names. |
| `TheDancingDeveloper-org/graft` | 12 commits | Trial write-ups on real repositories, ignore files honoured by its client, a status fix for resolved conflicts. |
| [CarterT27/scion](https://github.com/CarterT27/scion), [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion), [OpenCSGs/scion](https://github.com/OpenCSGs/scion), [feder-cr/git-ai](https://github.com/feder-cr/git-ai), `adrianchifor/scn`, `rizaust/open-scion`, `kianaaa19/scionhub` | none | Quiet. |

**Later reading, 05:20 UTC.** The GitHub API and repository pages still refused this session
(no stars, no search). Anonymous clones under the scratchpad, commit subjects only, since the
first run today: only `TheDancingDeveloper-org/graft` moved, with 8 commits: a one-command
repository onboarding in its deploy scripts (the first deploy waits for the new repository's
initial import), short changeset ids resolved by its CLI, a stale-base refusal that reports the
current tip and the paths involved, and more on its git-mirror batch PRs. No new tag anywhere
(the newest tags, `OpenCSGs/scion` `v0.1.10`, `adrianchifor/scn` `0.2.0` from 2026-10-17 and
`feder-cr/git-ai` `v0.4.0` from 2026-10-23, predate this run).

- **One-command onboarding** is the same goal as Scion's roadmap step 9.11 (five-minute
  quickstart, in progress); no issue, the step already covers it.
- **Short changeset ids** and **a stale-base refusal that names the current tip** are CLI
  presentation details worth checking against `scn` when Phase 9 polishes the quickstart;
  noted only.

### What it means for Scion

- **Numbers tied to a named run.** One project publishes its capacity figures with the id of
  the CI run that produced them. Scion's README table already names the commit, machine and
  timestamp of its run (written by `scripts/render-assets.py` from `bench-real.csv`); no
  change.
- **The concurrency claim is now measured.** The 2026-10-23 note said a 16-writer land chart
  waits until the benchmark exists. `land 16 writers` is now a row of `bench-real.csv` and of
  the README table; whether it earns a place on the first screen is a later call, and the
  first screen stays as the rules set it.
- **A "Try it" section that runs as written** is the technique behind one project's fix. Scion's
  Getting started is the same idea; keep its commands exact.
- **Ignore files** are a non-goal for Scion (`CLAUDE.md`); noted only.

## 2026-10-24

The GitHub search API answered this run, both for "scion source control" (sorted by last
update, 20 repositories, none new to these notes) and for repositories created since
2026-10-22 (none). `bnsd55/open-scion` no longer appears in search results but still answers
anonymous git reads, so its commits come from a clone under the scratchpad; its stars were not
read. Nothing below was read beyond each repository's page metadata and commit subjects.

| Repository | Stars / forks | Since the last run | Noted |
|---|---|---|---|
| [CarterT27/scion](https://github.com/CarterT27/scion) | 237 / 18 | no commits | Quiet since 2026-10-16. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 113 / 18 | no commits | Quiet since 2026-10-14. |
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 980 / 96 | last push 2026-10-23 | Built on git; its homepage field points at the Anthropic post. |
| [OpenCSGs/scion](https://github.com/OpenCSGs/scion) | 3 / 0 | 10 commits, tag `v0.1.10` | A release note per tag, then a design document and the work for a git gateway (clone, fetch, push over smart HTTP; partial and shallow clones). |
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) | not read | 26 commits | Tests, CI coverage per backend, a "production audit" with a v1.0 blocker list, and a roadmap line marking Postgres experimental. No tag. |
| `TheDancingDeveloper-org/graft` | 0 / 0 | updated 2026-10-24 | Still "design stage" in its description. |
| `adrianchifor/scn`, `rizaust/open-scion`, `kianaaa19/scionhub` | 1 / 3 / 0 | none | Quiet. |

**Later reading, 05:20 UTC.** The GitHub API and repository pages refused this session, so no
star counts and no search; commits come from anonymous clones under the scratchpad. Since the
first run today only [OpenCSGs/scion](https://github.com/OpenCSGs/scion) moved: four commits on
its git gateway (unservable commits reported as a plain git error before the pack, a hint to
import such repositories into a new one, packs planned oldest commit first so reverted changes
clone). No new tag. All other tracked repositories are unchanged.

### What it means for Scion

- **Release notes per tag** are the visible face of one project's cadence. Scion has no tag
  yet; when its first release comes, a short note per tag is worth copying as a technique.
- **A public "blocker list" before 1.0** is a presentation choice that states plainly what is
  missing. Scion's README says "alpha" and links `docs/ROADMAP.md`, which serves the same
  purpose; no change.
- **Git gateway:** the git wire protocol is a non-goal for Scion (`CLAUDE.md`); noted only.

## 2026-10-23

Collapsed (> 30 days): one project shipped ten tags in four days and a `watch` command (filed as #212); a sharp drop in one project's star count; no new project with ≥ 5 stars.

## 2026-10-22

Collapsed (> 30 days): API refused; one project still on operations and proof; graceful shutdown filed as #179.

## 2026-10-21

Collapsed (> 30 days): API refused; one project led on operations; backup/restore filed as #139.

## 2026-10-19

Collapsed (> 30 days): six new projects listed; presentation notes (link row, concurrency chart once measured, topics).

## 2026-10-18

Collapsed (> 30 days): first reading of three projects; topics, description and benchmark placement noted.
