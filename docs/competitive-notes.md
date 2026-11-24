# Competitive notes

Other open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/research/redesigning-source-control).
This file records only how they **present** themselves, to learn presentation technique.
Scion takes no code, design decision or architecture from them, and copies no text or image
from any of them (several are GPL-licensed). None of these names or links belong in the
README.

## 2026-11-24

First run since 2026-10-26. Repository search answered; per-repository API calls stayed
refused, so commits and tags come from anonymous partial clones under the scratchpad (subjects
only) and features from each README as the project states it. Pull-request titles are not
readable this way; commit subjects stand in for them. "24 h" is since 2026-11-23 06:00 UTC.

| Repository | Stars / forks (10-26 → today) | Since 10-26 / last 24 h | Noted |
|---|---|---|---|
| [feder-cr/git-ai](https://github.com/feder-cr/git-ai) | 988 → 1003 / 101 | 59 commits / 0 | **Tags `v0.4.1`, `v0.4.2`** (2026-11-05): a bounded, configurable Postgres pool, a benchmark of the commit rate of concurrent writers, refusal of ref-name pairs git cannot hold together, a conformance harness. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 245 → 320 / 27 | 0 / 0 | Default branch quiet since 2026-10-16; stars still growing fastest in absolute terms. |
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 119 → 160 / 24 | 0 on the default branch / 0 | Last push 2026-10-27 (not on the default branch); 14 open issues. |
| [bnsd55/openscion](https://github.com/bnsd55/openscion) | 30 → 30 / 5 | ~450 commits / **29** | **First release `v0.1.0`** (2026-11-01; README points at a signed tag, checksums and attestations). Last 24 h: one principal holds at most half of each shared server limit, a wait for a shared limit ends at a 30 s busy bound with an alert when such timeouts rise, an idle upload frees its slot, backup/restore/upgrade runbooks that run as written. Roadmap: v0.1.1 distribution, v0.2.0 N/N-1 compatibility and recovery, v0.3.0 72-hour soak and a checkout benchmark against `git clone`, v0.4.0 production boundary (threat model, support policy, OpenSSF badge). |
| `TheDancingDeveloper-org/graft` | 0 / 0 | ~245 commits / 0 | **Tag `v0.1.1`** (2026-10-31). Work since: git-mirror "batch" PRs settled on the server, a service-token kind that cannot delegate, the server's MCP mount refusing a credential-reading tool. AGPL-3.0: nothing copied. |
| [kianaaa19/scionhub](https://github.com/kianaaa19/scionhub) | 0 / 0 | 0 / 0 | Quiet since 2026-10-18. |
| `OpenCSGs/scion`, `hermes98761234/sgit` | – | – | `OpenCSGs/scion` no longer answers anonymously (private or removed); `sgit` quiet since 2026-10-15 and gone from search. |
| **new (≥ 5 stars):** [yuchenZhangTG/graft](https://github.com/yuchenZhangTG/graft) | 23 / 0 | 6 commits, created 2026-11-02, last 2026-11-05 | Apache-2.0/MIT, Rust. Claims a writable FUSE workspace mounted "in < 50 ms" (tagline: "<30 ms"), an AST/semantic three-way merge, a per-machine daemon with a remote blob store, SQLite + S3 locally, and a built-in web UI with code search. No published benchmark data behind the numbers. Unrelated to the AGPL project of the same name. |

Search for repositories created after 2026-10-13 found about twenty more Scion-inspired
projects, all under 5 stars (the largest has 3); not listed. Projects named "Scion" for
phones, satellites or an agent-skills app are unrelated.

**Direction.** Two themes this month. *Operating one server well*: one project shipped a first
release and spends its days on runbooks, fairness under load, soak tests and compatibility
across versions — the production-readiness path, not features. *Virtualized workspaces*: the
newest project leads with a lazily filled writable mount, the same direction as issue #295
(`scn mount`). Nobody added a git gateway, hosting or MCP since the last run; web UI with code
search appears in the newcomer (Scion's search idea is #298).

### What it means for Scion

- **Fair share of server capacity per principal.** One project caps any single principal at
  half of each shared limit and bounds the wait. Scion has no per-principal concurrency
  bound, and a hosted instance (Phase 9.9) serves many agents; #315 opened today (the
  roadmap's "quotas stay what the boundary enforces" sentence is the owner's call to revisit).
- **Release.** Three projects now ship tagged releases; Scion's is Phase 9.2 (in progress),
  provenance is #281. No new issue.
- **Writable mounts with millisecond claims.** #295 is the first step (read-only); the
  claim has no data behind it yet, so no benchmark gap.
- **Concurrent-writer commit-rate benchmark** (git-ai 0.4.x) is what `land 16 writers` in
  `bench-real.csv` already measures; no change.

### Weekly: ahead / behind (2026-11-24, for the week of 2026-11-23)

From each README and docs as stated today; ✓ stated, – absent or stated as not done, ? not
stated. Scion's column is from its own README and `docs/ROADMAP.md`.

| Feature | Scion | CarterT27 | tinyhumansai | bnsd55 | kianaaa19 | yuchenZhangTG | graft (AGPL) | feder-cr |
|---|---|---|---|---|---|---|---|---|
| CLI | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Server-side land | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Persistent store | ✓ Postgres + S3 | ✓ CockroachDB + S3 | ✓ SQLite, files | ✓ Postgres or files | ✓ SQLite | ✓ SQLite + S3 | ? | ✓ SQLite or Postgres |
| Derived data | ✓ | ✓ | ✓ | ✓ | ✓ | ? | ? | ✓ |
| Daemon / cache tier | – (Phase 4) | ✓ | ✓ | ✓ | – | ✓ | ✓ node | ✓ |
| Git import | ✓ | ✓ | ? | ✓ | ? | ? | ✓ | ✓ shadow mode |
| Git gateway | – non-goal | – | – | – | – | – | – export only | ? |
| Auth | ✓ GitHub, SPs, ACLs | ✓ tokens | ? | ✓ tokens, tickets | ✓ passwords, tokens | ? | ✓ tokens | ✓ per-agent tokens |
| Web UI | ✓ | ? | ✓ | ? | ✓ | ✓ + code search | ? | ? |
| HTTP API | ✓ | ? | ✓ | ? | ✓ | ✓ REST | ? | ? |
| MCP | ✓ | ? | ? | ? | – | ? | ✓ | ? |
| Benchmark | ✓ | ✓ | ✓ | ✓ | – | claims only | ✓ | ✓ |
| Release | – (Phase 9) | – | – | ✓ v0.1.0 | – | – | ✓ v0.1.1 | ✓ v0.4.2 |

**Ahead:** still the only project stating web UI, HTTP API and MCP together with GitHub
sign-in and directory ACLs on Postgres + S3. **Behind:** no daemon/cache tier (Phase 4;
six projects state one), no tagged release (three projects ship one), no writable or lazy
mount (#295), and no per-principal fairness under load (#315).

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

## Older runs (one line each)

- **2026-10-24:** API refused for part of the run; notes on release notes per tag, a public pre-1.0 blocker list, and a git gateway (non-goal).
- **2026-10-23:** two runs; many-agent landing demos on first screens, a first benchmark against another implementation, following `main` live (#212), a ten-tag release cadence elsewhere.
- **2026-10-22:** shutdown draining as the next operations gap (#179); runbooks tested by CI; no new benchmark claim.
- **2026-10-21:** operations (backup/restore, #139) as where the most active project leads; small web UIs and HTTP APIs elsewhere; no new benchmark claim.
- **2026-10-19:** light/dark banners, showing the concurrency claim, a link row under the badges, repository topics/description unset.
- **2026-10-18:** first run: project list and stars; repository topics and description, benchmarks near the top, "what it is not" early.
