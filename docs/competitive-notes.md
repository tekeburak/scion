# Competitive notes

Other open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/engineering/redesigning-source-control).
This file records only how they **present** themselves, to learn presentation technique.
Scion takes no code, design decision or architecture from them, and copies no text or image
from any of them (several are GPL-licensed). None of these names or links belong in the
README.

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
