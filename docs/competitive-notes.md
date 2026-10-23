# Competitive notes

Other open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/engineering/redesigning-source-control).
This file records only how they **present** themselves, to learn presentation technique.
Scion takes no code, design decision or architecture from them, and copies no text or image
from any of them (several are GPL-licensed). None of these names or links belong in the
README.

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
