# Competitive notes

Other open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/research/redesigning-source-control).
This file records only how they **present** themselves, to learn presentation technique.
Scion takes no code, design decision or architecture from them, and copies no text or image
from any of them (several are GPL-licensed). None of these names or links belong in the
README.

## 2026-10-27, second run

An earlier run today (#273) took star counts and searched for new projects. This run's GitHub
API refused every repository outside this one, so it adds no star rows and no search.
Commits and tags come from anonymous clones under the scratchpad, commit subjects only, for the
24 hours since the second run of 2026-10-26; features as each README states them.

| Repository | Since the last reading | Noted |
|---|---|---|
| `TheDancingDeveloper-org/graft` | ~126 commits, no new tag (`v0.1.0`) | A **post-land check mode**: an attester skips ahead, **bisects** the lands since the last green one, retries, and **reverts the first bad land**, the revert carrying the failed check run. Also HMAC-signed outbound webhooks from its land journal, review-queue routes and MCP tools for review and attestation, a GitHub OIDC token exchange for CI, shell completions, and release archives for Linux (musl) and macOS. AGPL-3.0: nothing copied. |
| [OpenCSGs/scion](https://github.com/OpenCSGs/scion) | 28 commits, **tag `v0.1.12`** | Release 0.1.12: "git's everyday commands" (`stash`, `clean`, `reflog`, `switch --detach`, `log --follow`, annotated tags through its git gateway), automatic incremental repack, then `scn mcp` (the repository as MCP tools, no checkout) and `scn workspace` (several working copies, like `git worktree`). README now links a production compose and a multi-node cluster guide. |
| [bnsd55/openscion](https://github.com/bnsd55/openscion) | 30 commits, no tag yet | A `release/v0.1.0` branch with release notes: the first tag looks close. Work is hardening: private data directories under any umask, the server refuses a group-readable one, checkout refuses `.`/NUL/`/` names from a hostile tree, fsck and mutation-testing passes. |
| [CarterT27/scion](https://github.com/CarterT27/scion), [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion), [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub), [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | none on the default branch | Quiet; no new tags. |

**Direction.** Two pulls this week: git-compatibility depth (one project now wraps most of
git's daily commands and a gateway; both stay Scion non-goals beyond what `scn` already does)
and **verification after land**. The post names automated bisect and revert after a post-merge
test failure as a tool the source-control API makes possible (noted on 2026-10-26); one
project now ships it. Scion has the pieces (`FindLanding`, `ListChangeRequests`, server-side
`Revert`, `scn checkout`) but no command that strings them together: #274 proposes
`scn bisect` over landed change requests as one step.

### What it means for Scion

- **Post-land bisect** → #274. It walks change requests, not the changeset log,
  so invariant 1 holds; the revert stays the existing `scn revert`.
- **Land webhooks** are the push form of #212 (`scn watch`); no new issue.
- **MCP without a checkout** is what `scion-mcp` already does (Phase 11.4); no change.
- **A first tag** is about to exist in a third project; Scion's Phase 9 release steps remain the
  gap the 2026-10-26 table names.

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

The repository pages answered this run (the API did not), so star and fork counts are back,
read from each page. The GitHub search for "scion source control", sorted by last update,
lists 20 repositories; nine are new to these notes. Nothing below was read beyond each
repository's page and README headings; `tinyhumansai/openscion` stays GPL-3.0-only.

| Repository | Stars / forks | First screen | Noted |
|---|---|---|---|
| [CarterT27/scion](https://github.com/CarterT27/scion) | 236 / 18 | title, tagline, non-affiliation paragraph, a star-history chart, then Benchmarks | Quiet since 2026-10-16; the star-history chart sits above the benchmarks. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 111 / 18 | social-preview image, title, one paragraph, then a benchmark chart against git on synthetic repositories | Quiet since 2026-10-14; no badges. |
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) | 29 / 2 | banner, four badges (CI, license, Rust version, "alpha"), an alpha notice, one-line purpose, a non-affiliation section | 34 commits since noon on 2026-10-22, all tests, CI matrix (Postgres majors, arm64, xfs), import written in batches, audit lines. No new README benchmark number. |
| [OpenCSGs/scion](https://github.com/OpenCSGs/scion) *new* | 3 / 0 | title, an English / Chinese switcher, an architecture diagram, a demo GIF (four agents landing on one branch, with an MP4 link), then benchmark tables | Apache-2.0. Its benchmark tables put git and another implementation of the design side by side with its own medians. Topics: grpc, rust, source-control, version-control. |
| [adrianchifor/scn](https://github.com/adrianchifor/scn) *new* | 1 / 0 | title and an SVG architecture diagram (stateless controllers, an index, SQLite shards, S3), then install and compose sections | No badges, no benchmark chart, no demo. |
| [rizaust/open-scion](https://github.com/rizaust/open-scion) | 3 | — | Last update about 2026-10-15. |
| [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) | 0 / 0 | — | Quiet since its single commit; no description or topics. |
| Seen in the search, not opened: `TheDancingDeveloper-org/graft` (0, design stage), `culebraluxe/forgevcs` (0), `Lvstw1205/scion-lite` (0), `Hanam824/scion` (0, a desktop app), `wrencanfly/wren` (0, built on git), `TomOrth/deltrove` (0, planning), `scholzdev/boughline` (0), `virajbhartiya/flux` (1), `mangreen/mini-scion` (1), `fenwii/openscion` (1) | | | All created in the ten days since the post, none with a release. |

### What it means for Scion

- **A demo of many agents landing on one branch** is now on the first screen of two projects
  (an animated banner on 2026-10-19, a GIF here). Scion's README shows a measured chart of the
  same thing (16 writers); no GIF is planned, since a recording would need its own script to
  stay reproducible like the SVGs.
- **Benchmarks against another implementation** appear for the first time in a README. Scion
  plans peers in its own benchmark (10.10, from the 6.10 decision); until then its table says
  "Other implementations of the design: not measured in this run", which stays true.
- **Status badge:** only `bnsd55/open-scion` and Scion carry an "alpha" badge; no change.

### 2026-10-23, second run (05:20 UTC)

The GitHub search API answered this run; per-repository API reads were refused, so commits
and tags come from anonymous clones under `/tmp`. Counts unchanged since the first run except
`feder-cr/git-ai`, which the search now reports at 967 stars and 93 forks (2,299 on
2026-10-19): a drop of more than half, as when an account's stars are removed in bulk.

| Repository | Last push | Last 24 h | Direction |
|---|---|---|---|
| [OpenCSGs/scion](https://github.com/OpenCSGs/scion) | 2026-10-23 | 32 commits, tags `v0.1.7`–`v0.1.9` (ten tags since 2026-10-20) | Client ergonomics around concurrent landing. New `scn watch` follows branches on the server and prints each commit as it lands; `scn revert` and `scn cherry-pick` make a pending commit; a stack of pending commits that `sync` rebases one by one; land conflicts name up to three paths; `status` says how far the server's branch has moved (behind N). Server rebase records the original commit in an extension so `log` shows "rebased from". Docker Hub images. README benchmark claims 2–12× faster than git on another project's synthetic workloads (one Apple M5, medians). |
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 2026-10-23 | 28 commits, tag `v0.3.1` | Built on git (reftable housekeeping, start-point resolution of branches); a measured study of conflict handling for agent PRs. Not the reference design's architecture. |
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) | 2026-10-23 | 47 commits | Same direction as in the first run: import and graph reads in batches, audit lines, CI matrix proven on real xfs, Postgres majors and arm64. No tag, no new README number. |
| `CarterT27/scion`, `tinyhumansai/openscion`, `kianaaa19/scionhub` | 2026-10-16 / 10-14 / 10-18 | none | Quiet. |

New in the search with five stars or more: none.

#### What it means for Scion

- **Following `main` live.** Scion has no way for an agent or CI job to wait for the next
  landed changeset other than polling `log`. A streaming read keyed by `generation` fits
  Phase 11.5 (agent ergonomics). Opened as one `idea:` issue.
- **Release cadence:** one project has shipped ten tags in four days; Scion's first release
  is 9.x. No change to the plan; noted.
- **Benchmarks:** one more README now claims a multiple over git on synthetic workloads;
  Scion's numbers stay from its real-repository benchmark (`docs/BENCHMARKS.md`).

## 2026-10-22

Star and fork counts of the watched projects are missing again: the GitHub API and the
repository pages refused this run, so only anonymous git reads worked (clones under `/tmp`).
Scion's own counts came from the API (0 stars, 1 fork). The search for new projects created
after 2026-10-13 could not run. No tags or releases on any of the four watched repositories.

| Repository | Last push | Last 24 h | Direction |
|---|---|---|---|
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) | 2026-10-22 | 48 merged PRs | Still operations and proof, no new feature. Postgres hardening (disk-full error mapping, crash tests, migrations logged), an audit log that records intent, result and every ref move, client keepalive that notices a dead peer in about 50 s, a measured restart time after a SIGKILL in the middle of a land, a measured restore time, runbooks for first boot and certificate rotation, a release-verification page for users, a security reviewer's starting packet, fuzzing of the archive readers (two bugs found), and a docs test that runs every command a runbook names as proof. Import now refuses dates git cannot write. No new benchmark number in the README; its many-agent land demo (`scn bench --agents`) is unchanged. |
| [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) | 2026-10-18 | none | Quiet since its single commit. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 2026-10-16 | none | Quiet for six days. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 2026-10-14 | none | Quiet for eight days. |

### What it means for Scion

- **Shutdown is the next operations gap.** The active project drains requests before it stops
  and measures recovery after a kill. `scion-server` stops at once on SIGTERM; land recovery
  (5.4) keeps the data safe, but every rolling deploy of the hosted instance (9.9) cuts
  in-flight clones and lands with a transport error. Opened as one `idea:` issue.
- **Runbooks tested by CI** are the pattern behind most of their PRs. Scion's 9.9 `HOSTING.md`
  and 9.11 quickstart already plan commands checked against the scripts; no change.
- **Benchmarks:** no new claim anywhere since 2026-10-19.

## 2026-10-21

Star and fork counts are missing today: the GitHub API token of the watch run was refused, so
only anonymous git reads worked (shallow clones under `/tmp`). The search for new projects
created after 2026-10-13 could not run either. `competitive-stars.csv` starts today, backfilled
from the star counts recorded on 2026-10-18 and 2026-10-19 (forks were not recorded). No tags exist on any of the four watched
repositories.

| Repository | Last push | Last 24 h | Direction |
|---|---|---|---|
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) (`bnsd55/openscion` redirects here) | 2026-10-21 | about 60 merged PRs | Operations before features. A versioned roadmap ending at "v1.0.0, single-server production". In one day: a logical archive (export, verify, restore) with a documented format, readiness and liveness endpoints with a bounded drain, an audit log, Prometheus alert rules with a runbook each, runbooks for a full disk, a failed migration and an fsck failure, a threat model, fuzzing nightly, CodeQL, OpenSSF Scorecard, an 80% coverage gate, reproducible and attested release artifacts, a land reference model, a kill-under-load soak test. `v0.1.0` still unreleased. |
| [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) *new* | 2026-10-18 | none (one commit in total) | A Node.js hosting hub, MIT: web UI (browse, history, blame, diff, in-browser edit, search, tokens), HTTP/JSON API, a JS `scn` CLI with sparse clone and `commit --propose`, change requests with discussion, SQLite single node. Has the web UI and HTTP API that Scion plans in Phase 11, but no daemon, no tiers, no git import, no benchmark. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 2026-10-16 | none | Quiet for five days. 23 crates (a CockroachDB store, a merge crate, redaction, policy). |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 2026-10-14 | none | Quiet for a week. GPL-3.0-only: nothing read beyond headings. Roadmap has Done / Next / Later sections and a web app under `apps/web`. |

### What it means for Scion

- **Operations are where the most active project now leads.** Scion has no backup and restore
  story for its Postgres metadata and no readiness/drain endpoint, and Phase 9 (hosting) does
  not list them. A hosted instance needs both. Opened as one `idea:` issue (#139, backup/restore).
- **A web UI and HTTP API exist elsewhere already** in a small single-node form. Scion's
  Phase 11 covers the same ground with the tiered server behind it; no change to the plan, but
  Phase 11 is no longer something nobody else has.
- **Benchmarks:** no new benchmark claim in any README since 2026-10-19.

## 2026-10-19

Star counts from the GitHub search API that day. New since 2026-10-18 are marked *new*.

| Repository | Stars | Language, license | First screen | Presentation technique worth noting |
|---|---:|---|---|---|
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) *new* | 2,299 | Go, MIT (its LICENSE also carries a GitLab copyright) | animated banner (GIF) with separate light, dark and phone variants through `<picture>`, then a one-line tagline | The banner animates the claim itself: many agents racing to land on one branch. Inspiration and non-affiliation note sit right under it, as a small `<sub>` line. Topics include `scion`, `coding-agents`, `version-control`. Went from created to 2,299 stars in four days. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 223 | Rust, Apache-2.0 | unchanged since 2026-10-18 | |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 96 | Rust (+ Next.js web GUI), GPL-3.0-only | unchanged since 2026-10-18 | |
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) | 27 | Rust, MIT OR Apache-2.0 | unchanged since 2026-10-18 | |
| [rizaust/open-scion](https://github.com/rizaust/open-scion) *new* | 2 | Python, Apache-2.0 | language switcher (English / Chinese), full-width cover SVG, a link row (website, get started, architecture, API, contributing), then badges | A "Why" section of seven bolded one-line benefits, each a verb phrase. Has a project website on GitHub Pages and ten topics. |
| [garyships/kudzu](https://github.com/garyships/kudzu) *new* | 1 | Go, MIT | logo, two-line tagline, five badges, a link row (website, demo video, design, a Chinese README), then a demo GIF | A "what breaks / what we do" table as the first section, one row per git pain point. |
| [virajbhartiya/flux](https://github.com/virajbhartiya/flux) *new* | 1 | Rust, Apache-2.0 | title, one bolded sentence, eight bolded bullets, then screenshots of a web UI | A "Speed" table of single numbers measured on one named laptop against a well-known public repository. |
| [huzjie/scionforge](https://github.com/huzjie/scionforge) *new* | 0 | Python, MIT | Chinese-language README, a quoted one-sentence summary | A git vs changeset comparison table near the top. |
| [VishiATChoudhary/graft](https://github.com/VishiATChoudhary/graft) | 1 | Go | not reviewed | |
| [voduff/nugit](https://github.com/voduff/nugit) | 0 | Rust | not reviewed | |

### What Scion could learn

- **Light and dark banner variants.** The largest project serves a different banner per color
  scheme with `<picture>`. Scion's SVGs are drawn to read on both GitHub themes from one file,
  which keeps one source of truth; nothing to change.
- **Show the concurrency claim.** Two new projects lead with many writers landing at once.
  Scion has not measured that yet; once the planned 16-writer land benchmark exists, its chart
  is a candidate for the first screen. Until then it stays out of the README.
- **A link row under the badges** (architecture, data model, FAQ, contributing) is common to
  three of the new projects and cheap to add.
- **Repository topics and description** are still unset / out of date on Scion's GitHub page
  (see 2026-10-18); both are repository settings, not files.

## 2026-10-18

Star counts as shown on each repository page that day.

| Repository | Stars | Language, license | First screen | Presentation technique worth noting |
|---|---:|---|---|---|
| [CarterT27/scion](https://github.com/CarterT27/scion) | 219 | Rust, Apache-2.0 | title and one paragraph, then a star-history chart; no logo, no badges | Benchmarks are the second section, split into "real repository with persistent storage" and "concurrent CI workloads"; numbers are labelled as measured on one named machine. No topics set. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 95 | Rust (+ Next.js web GUI), GPL-3.0-only | banner image, then a benchmark SVG against git (import, full clone, single-file commit, single-directory clone) | Leads with a benchmark chart before "Why"; also has "Releasing" and "Contributing" sections. Architecture is a text diagram. |
| [bnsd55/open-scion](https://github.com/bnsd55/open-scion) | 25 | Rust, MIT OR Apache-2.0 | banner logo, badges (CI, license, Rust version, "proof of concept" status), one paragraph | A "What this is, and what it is not" section right after the intro; nine topics (ai-agents, content-addressable, git, grpc, monorepo, rust, source-control, vcs, version-control); demo as a "Try it" shell section. |
| [VishiATChoudhary/graft](https://github.com/VishiATChoudhary/graft) | 1 | Go | not reviewed | Description names the post directly. |
| [voduff/nugit](https://github.com/voduff/nugit) | 0 | Rust | not reviewed | Description names the post directly. |

Not related despite the name: GoogleCloudPlatform/scion (agent orchestration) and
annex-ai/scion (an AI agent).

### What Scion could learn

- **Repository topics.** The only project with GitHub topics set is easier to find; Scion's
  repository has none. Setting topics is a repository setting, not a file in this repo.
- **Repository description.** Scion's GitHub description still says "pre-MVP", which the
  README no longer uses; the README calls the project alpha. Also a repository setting.
- **Benchmarks near the top.** Two of the larger projects show measured numbers on the first
  screen. Scion's measured chart (`docs/assets/bench-real.svg`) sits lower on the page; the
  first screen stays logo, one sentence, badges, the inspiration note, "Why Scion" and the
  architecture figure.
- **"What it is not" early.** Scion's Scope section covers this; it is further down.
