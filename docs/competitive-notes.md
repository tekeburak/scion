# Competitive notes

Other open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/engineering/redesigning-source-control).
This file records only how they **present** themselves, to learn presentation technique.
Scion takes no code, design decision or architecture from them, and copies no text or image
from any of them (several are GPL-licensed). None of these names or links belong in the
README.

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
