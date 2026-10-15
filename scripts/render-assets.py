#!/usr/bin/env python3
"""Render the README / docs figures in docs/assets/ as SVG.

Standard library only, no randomness, no timestamps: running it twice produces identical
bytes, so a diff in docs/assets/ always means the figure changed on purpose.

    python3 scripts/render-assets.py            # writes docs/assets/*.svg
    python3 scripts/render-assets.py --check    # exits 1 if any file is out of date

Every figure is a schematic of what docs/ARCHITECTURE.md and docs/DATA_MODEL.md describe.
The only number drawn is the one a test asserts (see `changeset_svg`). Update ROADMAP below
when a phase changes status; nothing else here should need touching for that.
"""

from __future__ import annotations

import sys
from pathlib import Path
from xml.sax.saxutils import escape

OUT = Path(__file__).resolve().parent.parent / "docs" / "assets"
W = 1600

# One palette for every figure. Dark card on a transparent page, so it reads on both of
# GitHub's themes; every text colour clears WCAG AA against BG.
BG = "#0d1321"
PANEL = "#151d2e"
LINE = "#2a3650"
TEXT = "#e8edf5"
MUTED = "#93a1b8"
TEAL = "#2dd4bf"    # object plane, content-addressed data
AMBER = "#f5b545"   # control plane, metadata
VIOLET = "#a78bfa"  # derived data
GREEN = "#4ade80"   # done
FONT = "Inter, 'Segoe UI', -apple-system, BlinkMacSystemFont, Helvetica, Arial, sans-serif"
MONO = "'JetBrains Mono', 'SFMono-Regular', Menlo, Consolas, 'Liberation Mono', monospace"

# Roadmap status. Source of truth is the README roadmap and the merged "Phase N.M:" PRs.
ROADMAP = [
    ("0", "Workspace, CI, local stack", "done"),
    ("1", "Data model, Postgres + S3 stores", "done"),
    ("2", "Derived data, diff, merge, rebase", "done"),
    ("3", "scion-server + scn CLI (MVP)", "done"),
    ("6", "git import into Scion", "active"),
    ("4", "Cache and daemon tiers", "planned"),
    ("5", "Change requests, land worker", "planned"),
    ("7", "IAM, fuzzing, simulation", "planned"),
    ("8", "Virtual checkouts (FUSE / NFS)", "planned"),
]
# Phases run in the order listed: git import comes right after the MVP so real repositories
# can be tested and benchmarked.
STATUS_DONE = "MVP done: scion-server on Postgres + S3, scn clone --sparse, status, log, diff, blame, add, commit, land"
STATUS_NEXT = "in progress: Phase 6 git import (6.1-6.10 merged; 6.11-6.14 next: match git on the real-repo slice, publish results), then 4, 5, 7, 8"


def t(x, y, s, size=20, fill=TEXT, weight=400, anchor="start", mono=False, extra=""):
    family = MONO if mono else FONT
    return (
        f'<text x="{x}" y="{y}" font-family="{family}" font-size="{size}" fill="{fill}" '
        f'font-weight="{weight}" text-anchor="{anchor}"{extra}>{escape(s)}</text>'
    )


def box(x, y, w, h, stroke=LINE, fill=PANEL, r=14, dashed=False, sw=2):
    dash = ' stroke-dasharray="8 6"' if dashed else ""
    return (
        f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}" fill="{fill}" '
        f'stroke="{stroke}" stroke-width="{sw}"{dash}/>'
    )


def arrow(x1, y1, x2, y2, color, dashed=False, width=3):
    dash = ' stroke-dasharray="10 7"' if dashed else ""
    marker = {TEAL: "a-teal", AMBER: "a-amber", VIOLET: "a-violet", MUTED: "a-muted"}[color]
    return (
        f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="{color}" '
        f'stroke-width="{width}"{dash} marker-end="url(#{marker})"/>'
    )


def path_arrow(d, color, dashed=False, width=3):
    dash = ' stroke-dasharray="10 7"' if dashed else ""
    marker = {TEAL: "a-teal", AMBER: "a-amber", VIOLET: "a-violet", MUTED: "a-muted"}[color]
    return (
        f'<path d="{d}" fill="none" stroke="{color}" stroke-width="{width}"{dash} '
        f'marker-end="url(#{marker})"/>'
    )


def svg(h, title, body):
    markers = "".join(
        f'<marker id="{mid}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" '
        f'markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" '
        f'fill="{c}"/></marker>'
        for mid, c in (("a-teal", TEAL), ("a-amber", AMBER), ("a-violet", VIOLET), ("a-muted", MUTED))
    )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{h}" '
        f'viewBox="0 0 {W} {h}" role="img" aria-label="{escape(title)}">\n'
        f"<title>{escape(title)}</title>\n<defs>{markers}"
        f'<linearGradient id="glow" x1="0" y1="0" x2="1" y2="1">'
        f'<stop offset="0" stop-color="#16213a"/><stop offset="1" stop-color="{BG}"/>'
        f"</linearGradient></defs>\n"
        f'<rect x="0" y="0" width="{W}" height="{h}" rx="24" fill="url(#glow)"/>\n'
        + "\n".join(body)
        + "\n</svg>\n"
    )


def banner_svg():
    h = 400
    b = []
    # A first-parent line of changesets with one merge, drawn as the mark.
    xs = [940, 1060, 1180, 1300, 1420]
    y = 200
    b.append(f'<line x1="{xs[0]}" y1="{y}" x2="{xs[-1]}" y2="{y}" stroke="{AMBER}" stroke-width="4"/>')
    b.append(path_arrow(f"M{xs[1]},{y} C{xs[1] + 40},{y - 110} {xs[2] + 60},{y - 110} {xs[3] - 14},{y - 12}",
                        MUTED, dashed=True, width=2))
    for x in xs:
        b.append(f'<circle cx="{x}" cy="{y}" r="16" fill="{BG}" stroke="{AMBER}" stroke-width="4"/>')
    # derived trees hanging off two changesets
    for x in (xs[2], xs[4]):
        b.append(f'<line x1="{x}" y1="{y + 18}" x2="{x}" y2="{y + 62}" stroke="{VIOLET}" stroke-width="2"/>')
        b.append(f'<polygon points="{x},{y + 62} {x - 26},{y + 104} {x + 26},{y + 104}" fill="none" '
                 f'stroke="{VIOLET}" stroke-width="3"/>')
        for dx in (-34, 0, 34):
            b.append(f'<line x1="{x}" y1="{y + 104}" x2="{x + dx}" y2="{y + 132}" stroke="{TEAL}" stroke-width="2"/>')
            b.append(f'<rect x="{x + dx - 9}" y="{y + 132}" width="18" height="18" rx="3" fill="{TEAL}"/>')
    b.append(t(xs[0], y - 34, "main", 18, AMBER, 600, "middle", mono=True))
    b.append(t(1180, 372, "changesets · derived trees · content-addressed blobs", 18, MUTED, 400, "middle"))

    b.append(t(96, 170, "Scion", 104, TEXT, 800, extra=' letter-spacing="-2"'))
    b.append(t(100, 228, "Client-server source control", 34, TEXT, 500))
    b.append(t(100, 272, "for agent-scale monorepos.", 34, TEXT, 500))
    b.append(t(100, 334, "Rust · Postgres + S3 · BLAKE3 canonical encoding · Apache-2.0", 20, MUTED))
    return svg(h, "Scion: client-server source control for agent-scale monorepos", b)


def architecture_svg():
    h = 760
    b = []
    b.append(t(64, 72, "Architecture", 34, TEXT, 700))
    b.append(t(64, 108, "Two planes. The control plane is served only by scion-server; the object plane is the same "
                        "ObjectService at every tier, strictly daemon → cache → server.", 19, MUTED))

    # clients
    b.append(box(64, 190, 300, 120, stroke=LINE))
    b.append(t(214, 240, "developer machines", 22, TEXT, 600, "middle"))
    b.append(t(214, 272, "scn · sparse working copy", 17, MUTED, 400, "middle", mono=True))
    b.append(box(64, 470, 300, 120, stroke=LINE))
    b.append(t(214, 520, "CI / cluster jobs", 22, TEXT, 600, "middle"))
    b.append(t(214, 552, "agents · research jobs", 17, MUTED, 400, "middle", mono=True))

    # tiers
    b.append(box(470, 470, 250, 120, stroke=TEAL, dashed=True))
    b.append(t(595, 518, "scion-daemon", 22, TEXT, 700, "middle", mono=True))
    b.append(t(595, 548, "per node · Phase 4", 16, MUTED, 400, "middle"))
    b.append(t(595, 572, "seeds from snapshot manifest", 15, MUTED, 400, "middle"))
    b.append(box(810, 330, 260, 120, stroke=TEAL, dashed=True))
    b.append(t(940, 378, "scion-cache", 22, TEXT, 700, "middle", mono=True))
    b.append(t(940, 408, "regional · Phase 4", 16, MUTED, 400, "middle"))
    b.append(t(940, 432, "memory + disk", 15, MUTED, 400, "middle"))

    b.append(box(1150, 190, 380, 260, stroke=AMBER, sw=3))
    b.append(t(1340, 238, "scion-server", 26, TEXT, 700, "middle", mono=True))
    b.append(t(1340, 268, "authoritative", 17, MUTED, 400, "middle"))
    for i, s in enumerate(["control-plane RPCs", "object origin", "derived-data computation",
                           "serial land worker (Phase 5)"]):
        b.append(t(1180, 312 + i * 32, "• " + s, 18, TEXT))

    # stores
    b.append(box(1150, 560, 180, 150, stroke=AMBER))
    b.append(t(1240, 600, "Postgres", 22, TEXT, 700, "middle"))
    for i, s in enumerate(["changeset WAL", "file_changes", "refs (CAS)", "derived pointers"]):
        b.append(t(1240, 630 + i * 21, s, 14, MUTED, 400, "middle", mono=True))
    b.append(box(1350, 560, 180, 150, stroke=TEAL))
    b.append(t(1440, 600, "object store", 22, TEXT, 700, "middle"))
    for i, s in enumerate(["blobs/{id}", "trees/{id}", "snapshots/…", "S3-compatible"]):
        b.append(t(1440, 630 + i * 21, s, 14, MUTED, 400, "middle", mono=True))

    # control plane: both clients -> server
    b.append(arrow(364, 228, 1144, 228, AMBER))
    b.append(t(560, 214, "control plane: changesets, refs, log, diff, blame, land", 17, AMBER, 600))
    # routed below the tiers so it never crosses a cache box: control RPCs skip every tier
    b.append(path_arrow("M364,580 C410,580 400,630 450,630 L1080,630 C1110,630 1110,610 1110,580 "
                        "L1110,460 C1110,435 1120,425 1144,425", AMBER, width=2.5))

    # object plane
    b.append(path_arrow("M364,282 C560,282 640,390 804,390", TEAL))
    b.append(arrow(364, 560, 464, 560, TEAL))
    b.append(path_arrow("M720,530 C770,530 760,430 804,430", TEAL))
    b.append(path_arrow("M1070,390 L1144,390", TEAL))
    b.append(t(64, 360, "object plane: trees, blobs by id", 17, TEAL, 600))
    b.append(t(64, 384, "immutable GETs, cacheable at any tier", 15, MUTED))

    b.append(arrow(1240, 450, 1240, 554, AMBER))
    b.append(arrow(1440, 450, 1440, 554, TEAL))

    # legend
    lx, ly = 64, 668
    b.append(f'<line x1="{lx}" y1="{ly}" x2="{lx + 40}" y2="{ly}" stroke="{AMBER}" stroke-width="3"/>')
    b.append(t(lx + 52, ly + 6, "control plane", 16, MUTED))
    b.append(f'<line x1="{lx + 200}" y1="{ly}" x2="{lx + 240}" y2="{ly}" stroke="{TEAL}" stroke-width="3"/>')
    b.append(t(lx + 252, ly + 6, "object plane", 16, MUTED))
    b.append(box(lx + 392, ly - 12, 40, 24, stroke=TEAL, r=6, dashed=True, sw=2))
    b.append(t(lx + 444, ly + 6, "planned tier", 16, MUTED))
    b.append(t(lx, ly + 46, "In the MVP (Phase 3) both planes go to scion-server directly; no tier holds a "
                            "materialized copy of the repository.", 16, MUTED))
    return svg(h, "Scion architecture: clients, daemon, cache, server, Postgres and object store", b)


def changeset_svg():
    h = 780
    b = []
    b.append(t(64, 72, "The shape of a changeset", 34, TEXT, 700))
    b.append(t(64, 108, "Only what changed against the first parent is hashed. Trees, history and blame are "
                        "derived from it and stored beside it.", 19, MUTED))

    # changeset card
    cx, cy, cw = 64, 150, 560
    fields = [
        ("repo", "str"), ("parents", "seq(ChangesetId)  first = mainline"),
        ("author, author_date", "str, str · i64, i32"), ("committer, committer_date", "opt(…)"),
        ("message", "str"), ("file_changes", "map(path → file_change)"), ("extra", "map(str → bytes)"),
    ]
    ch = 96 + len(fields) * 44 + 92
    b.append(box(cx, cy, cw, ch, stroke=AMBER, sw=3))
    b.append(t(cx + 28, cy + 48, "Changeset", 26, TEXT, 700))
    b.append(t(cx + cw - 28, cy + 48, "SCCS v1", 17, AMBER, 600, "end", mono=True))
    b.append(t(cx + 28, cy + 76, "id = blake3(canonical(changeset))", 15, MUTED, 400, mono=True))
    for i, (k, v) in enumerate(fields):
        y = cy + 122 + i * 44
        hl = k == "file_changes"
        if hl:
            b.append(f'<rect x="{cx + 14}" y="{y - 28}" width="{cw - 28}" height="40" rx="8" fill="#1d2840"/>')
        b.append(t(cx + 28, y, k, 18, AMBER if hl else TEXT, 600, mono=True))
        b.append(t(cx + cw - 28, y, v, 15, MUTED, 400, "end", mono=True))
    ny = cy + 122 + len(fields) * 44 + 4
    b.append(t(cx + 28, ny, "✗ no tree pointer   ✗ no signature   ✗ no ref", 17, MUTED, 600))
    b.append(t(cx + 28, ny + 30, "signatures are detached, computed over the id", 15, MUTED))

    # file changes
    fx, fy = 700, 150
    rows = [
        ("src/cache/tier.rs", "Tracked", "regular, 18 KiB, copy_from"),
        ("tools/run.sh", "Tracked", "executable"),
        ("docs/old.md", "Deleted", ""),
    ]
    b.append(t(fx, fy + 22, "file_changes", 20, AMBER, 700, mono=True))
    for i, (p, kind, meta) in enumerate(rows):
        y = fy + 48 + i * 84
        b.append(box(fx, y, 420, 66, r=10))
        b.append(t(fx + 18, y + 28, p, 17, TEXT, 600, mono=True))
        b.append(t(fx + 18, y + 52, kind + ("  · " + meta if meta else ""), 14, MUTED, 400, mono=True))
        if kind == "Tracked":
            b.append(arrow(fx + 420, y + 33, 1226, fy + 82 + i * 70, TEAL, width=2.5))
    b.append(path_arrow(f"M{cx + cw},{cy + 122 + 5 * 44 - 8} C{cx + cw + 40},{cy + 122 + 5 * 44 - 8} "
                        f"{fx - 50},{fy + 81} {fx - 6},{fy + 81}", AMBER, width=2.5))

    # blobs
    bx = 1232
    b.append(t(bx, fy + 22, "blobs/{id}", 20, TEAL, 700, mono=True))
    for i, lab in enumerate(["blake3(bytes)", "blake3(bytes)"]):
        y = fy + 52 + i * 70
        b.append(box(bx, y, 300, 56, stroke=TEAL, r=10))
        b.append(t(bx + 150, y + 35, lab, 16, TEXT, 500, "middle", mono=True))
    b.append(t(bx, fy + 216, "immutable, zstd, object store", 15, MUTED))

    # derived data
    dy = 470
    b.append(box(700, dy, 832, 260, stroke=VIOLET, r=14))
    b.append(t(728, dy + 44, "Derived data", 24, TEXT, 700))
    b.append(t(728, dy + 72, "derive(changeset, value for each parent) → value", 16, MUTED, 400, mono=True))
    # small tree drawing
    tx, ty = 820, dy + 112
    nodes = [(tx, ty), (tx - 60, ty + 52), (tx + 60, ty + 52), (tx + 20, ty + 104), (tx + 100, ty + 104)]
    edges = [(0, 1), (0, 2), (2, 3), (2, 4)]
    for a, c in edges:
        b.append(f'<line x1="{nodes[a][0]}" y1="{nodes[a][1]}" x2="{nodes[c][0]}" y2="{nodes[c][1]}" '
                 f'stroke="{VIOLET}" stroke-width="2"/>')
    for i, (x, y) in enumerate(nodes):
        changed = i in (0, 2, 4)
        b.append(f'<circle cx="{x}" cy="{y}" r="13" fill="{VIOLET if changed else PANEL}" '
                 f'stroke="{VIOLET}" stroke-width="2.5"/>')
    b.append(t(tx, dy + 246, "filled = rewritten", 14, MUTED, 400, "middle"))
    types = [("RootTree", "trees/{id}; pointer in derived table"), ("PathHistory", "per-path last change"),
             ("Blame", "line → changeset"), ("SkipList", "bounded ancestry, merge-base")]
    for i, (k, v) in enumerate(types):
        y = dy + 118 + i * 34
        b.append(t(980, y, k, 18, VIOLET, 700, mono=True))
        b.append(t(1130, y, v, 16, MUTED))
    b.append(path_arrow(f"M{cx + cw},{cy + ch - 60} C{cx + cw + 50},{cy + ch - 60} 640,{dy + 130} 694,{dy + 130}",
                        VIOLET, dashed=True, width=2.5))

    # the one measured number, asserted by
    # crates/scion-derived/src/root_tree.rs::rewritten_nodes_bounded_by_depth_not_file_count
    b.append(box(64, 666, 560, 90, stroke=GREEN, r=12))
    b.append(t(88, 702, "One change in a 2,500-file tree rewrites 5 tree nodes:", 18, TEXT, 600))
    b.append(t(88, 730, "the directories on its path (test-asserted, scion-derived)", 16, MUTED))
    return svg(h, "The shape of a Scion changeset, its file changes, blobs and derived data", b)


def roadmap_svg():
    h = 312
    b = []
    b.append(t(64, 66, "Status", 30, TEXT, 700))
    colors = {"done": GREEN, "active": AMBER, "planned": LINE}
    labels = {"done": "done", "active": "in progress", "planned": "planned"}
    n = len(ROADMAP)
    gap = 12
    cw = (W - 128 - gap * (n - 1)) / n
    for i, (num, scope, status) in enumerate(ROADMAP):
        x = 64 + i * (cw + gap)
        c = colors[status]
        fill = "#16301f" if status == "done" else ("#33270f" if status == "active" else PANEL)
        b.append(box(round(x, 1), 96, round(cw, 1), 124, stroke=c, fill=fill, r=12,
                     dashed=status == "planned"))
        b.append(t(round(x + 16, 1), 134, f"Phase {num}", 19, TEXT, 700))
        b.append(t(round(x + 16, 1), 160, labels[status], 15, c if status != "planned" else MUTED, 600))
        words, lines, cur = scope.split(), [], ""
        for w in words:
            if len(cur) + len(w) + 1 > 19 and cur:
                lines.append(cur)
                cur = w
            else:
                cur = (cur + " " + w).strip()
        lines.append(cur)
        for j, ln in enumerate(lines[:2]):
            b.append(t(round(x + 16, 1), 184 + j * 18, ln, 13, MUTED))
    b.append(t(64, 260, STATUS_DONE, 16, TEXT))
    b.append(t(64, 286, STATUS_NEXT, 16, AMBER, 600))
    return svg(h, "Scion roadmap status by phase", b)


def badge_svg(label, value, color):
    # Flat badge in the same shape as GitHub's own workflow badge, so the row lines up.
    # Width is estimated at 6.6 px per character of 11 px Verdana; generous padding absorbs
    # the error.
    lw = round(len(label) * 6.6 + 12)
    vw = round(len(value) * 6.6 + 12)
    w = lw + vw
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="20" role="img" '
        f'aria-label="{escape(label)}: {escape(value)}"><title>{escape(label)}: {escape(value)}</title>'
        f'<clipPath id="r"><rect width="{w}" height="20" rx="3"/></clipPath><g clip-path="url(#r)">'
        f'<rect width="{lw}" height="20" fill="#555"/><rect x="{lw}" width="{vw}" height="20" fill="{color}"/>'
        f'</g><g fill="#fff" text-anchor="middle" font-family="Verdana, DejaVu Sans, sans-serif" font-size="11">'
        f'<text x="{lw / 2}" y="14">{escape(label)}</text><text x="{lw + vw / 2}" y="14">{escape(value)}</text>'
        f"</g></svg>\n"
    )


FIGURES = {
    "banner.svg": banner_svg,
    "architecture.svg": architecture_svg,
    "changeset.svg": changeset_svg,
    "roadmap.svg": roadmap_svg,
    "badge-status.svg": lambda: badge_svg("status", "MVP \u00b7 Phase 6 in progress", "#c27c0e"),
    "badge-license.svg": lambda: badge_svg("license", "Apache-2.0", "#2b7bb9"),
}


def main() -> int:
    check = "--check" in sys.argv[1:]
    OUT.mkdir(parents=True, exist_ok=True)
    stale = []
    for name, render in FIGURES.items():
        path = OUT / name
        data = render()
        if check:
            if not path.exists() or path.read_text(encoding="utf-8") != data:
                stale.append(name)
        else:
            path.write_text(data, encoding="utf-8")
    if stale:
        print("out of date, run scripts/render-assets.py: " + ", ".join(stale), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
