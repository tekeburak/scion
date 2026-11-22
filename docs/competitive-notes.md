# Competitive notes

Other open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/research/redesigning-source-control).
This file records only how they **present** themselves, to learn presentation technique.
Scion takes no code, design decision or architecture from them, and copies no text or image
from any of them (several are GPL-licensed). None of these names or links belong in the
README.

## 2026-11-22

Repository search answered; per-repository API calls stayed refused, so commits come from
anonymous partial clones under the scratchpad (subjects only), tags from `git ls-remote`, and
features from each README as the project states it. Window for "last 24 h": since 2026-11-21
06:15 UTC. The readings of 2026-11-09 to 2026-11-21 are in open notes PRs that CI has not let
merge (see below), so the table compares with the 2026-11-21 reading recorded there.

| Repository | Stars / forks (11-21 → today) | Last push | Last 24 h | Noted |
|---|---|---|---|---|
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 1,002 → 1,004 / 101 | 2026-11-22 | no commit on the default branch (a branch push only) | Tags still end at `v0.4.2` (2026-11-05). README benchmark unchanged since 2026-10-20: 50 agents × 5 changes on one branch, 1,487–1,944 lands per minute with batching against 264–385 without. |
| [bnsd55/openscion](https://github.com/bnsd55/openscion) | 30 / 5 | 2026-11-21 04:08 | none (last commit just before the window) | ~420 commits since 2026-10-26, all tests, docs-against-code audits and soak runs. `v0.1.0` (2026-11-01) is still the only tag; the README now points to a page for verifying a release (signed tag, checksums, attestations), runs protocol transcripts against the previous release, and lists a read-only NFS `scn mount` as experimental. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 316 → 317 / 25 → 26 | 2026-10-16 | none | Quiet five weeks; no tags. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 158 / 22 → 23 | 2026-10-27 | none | Quiet; no tags. GPL-3.0: nothing read beyond the description. |
| [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) | 0 / 0 | 2026-10-18 | none | Dormant. |
| [yuchenZhangTG/graft](https://github.com/yuchenZhangTG/graft) | 23 / 0 | 2026-11-05 | none | No tags; README claims unchanged (FUSE workspaces, syntax-tree merge, no benchmark data). |
| `TheDancingDeveloper-org/graft` | 0 / 0 | 2026-11-21 01:29 | none | Tags `v0.1.0`, `v0.1.1`. AGPL-3.0: nothing copied. |

`OpenCSGs/scion` no longer appears in search and `git ls-remote` asks for credentials: deleted
or made private. Search found no new source-control project created after 2026-10-13 with
≥ 5 stars; the only newcomer on the topic is `stigenai/scion` (0 stars, created 2026-10-27,
"server-first source control", Rust server and client), not tracked. Higher-starred
`Scion-Project/*` results are phone firmware, unrelated.

### What it means for Scion

- **Nothing moved in the last 24 hours** on any watched default branch; no new tag, release or
  benchmark claim.
- **Release verification** (signed tag, checksums, attestations) is on one project's README;
  Scion's 9.2 covers checksums and #281 covers attestation. No new issue.
- **Read-only mount** is now listed by one project as experimental; #295 already proposes it.
- **CI blocks the notes.** `cargo-deny` fails on every branch on advisories against locked
  dependencies (pulldown-cmark, rmcp, zstd); the dependency PR #305 is itself red, so notes PRs
  since 2026-11-09 are open and unmerged, each based on `main` alone. This PR will be blocked
  the same way until the advisories are fixed on `main`.
- No issue opened this run: no candidate without an existing `idea:` issue.

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

Collapsed (> 30 days): API refused; one project still on operations and proof; no tags anywhere.

## 2026-10-21

Collapsed (> 30 days): API refused; one project led on operations; backup/restore filed as #139.

## 2026-10-19

Collapsed (> 30 days): six new projects listed; presentation notes (link row, concurrency chart once measured, topics).

## 2026-10-18

Collapsed (> 30 days): first reading of three projects; topics, description and benchmark placement noted.
