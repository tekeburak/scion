# Competitive notes

Public open-source projects inspired by Anthropic's
[*Redesigning source control at Anthropic*](https://www.anthropic.com/engineering/redesigning-source-control).
We study how they present themselves and how fast they are. Our code is written on our own
types and invariants; nothing is taken from GPL-licensed repositories.

## 2026-10-15

Searched: "openscion", "open-scion", "graft", "nugit", and GitHub/web searches for projects
citing the post. "graft" matched only an unrelated tree-sitter codebase indexer for agents;
"nugit" and "open-scion" returned nothing relevant.

| Repo | Stars | License | Description / topics | Presentation technique worth noting |
|---|---:|---|---|---|
| [tinyhumansai/openscion](https://github.com/tinyhumansai/openscion) | 44 | GPL-3.0 | "Server-centric source control system for very large monorepos", based on Anthropic's Scion. No topics. | Banner image first, then a benchmark chart (SVG) vs git directly under the one-paragraph intro, with a pointer to methodology and raw numbers. Separate ROADMAP, CONTRIBUTING, SECURITY, SUPPORT, CODE_OF_CONDUCT files; docs/ split into ADRs, specs, plans. No badges. |
| [CarterT27/scion](https://github.com/CarterT27/scion) | 99 | Apache-2.0 | "Implementation of Anthropic's version control system". No topics. | One-line tagline, explicit non-affiliation sentence, then a benchmark **table** with full environment (hardware, OS, toolchain, commit, run count) and an honest note where git wins; copy-paste commands to reproduce. Later sections: how it works, scope, build, use, test, project map. No badges. |

Takeaways for this repo (presentation only):

- Both lead with numbers right after the intro. We have none measured yet; the Phase 6
  benchmark is where ours belongs. Until then the first screen should show status and design.
- Publishing the exact measurement environment and the cases where git is faster builds trust.
- Neither uses GitHub topics or a status badge; setting topics (`source-control`, `monorepo`,
  `rust`, `version-control`) is a cheap discoverability win for us.
