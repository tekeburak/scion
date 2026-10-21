//! The pages' HTML. Everything from the repository (names, paths, messages,
//! file contents, READMEs) goes through maud's escaping or, for a rendered
//! README, through [`markdown`], which drops raw HTML and unsafe links.

use std::fmt::Write as _;

use axum::http::StatusCode;
use maud::{DOCTYPE, Markup, PreEscaped, html};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};
use scion_core::DiffEntry;
use scion_store::ChangeRequest;
use scion_types::{Changeset, ChangesetId, FileType, RepoPath, Tree, TreeEntry};

use crate::{Acted, Action, Content};

const STYLE: &str = "
body{font:14px/1.5 system-ui,sans-serif;margin:0;color:#1f2328;background:#fff}
header{padding:10px 24px;border-bottom:1px solid #d0d7de;background:#f6f8fa}
header a{font-weight:600;color:inherit;text-decoration:none}
main{max-width:1100px;margin:0 auto;padding:16px 24px}
a{color:#0969da}
nav.crumbs{margin-bottom:12px}
nav.tabs a{margin-right:16px}
table{border-collapse:collapse;width:100%}
td,th{padding:4px 8px;border-bottom:1px solid #eaeef2;text-align:left;vertical-align:top}
pre,code,.mono{font-family:ui-monospace,monospace;font-size:13px}
pre{background:#f6f8fa;padding:12px;overflow:auto}
pre.diff{padding:0}
pre.diff span{display:block;padding:0 12px}
.add{background:#dafbe1}.del{background:#ffebe9}.hunk{color:#57606a;background:#ddf4ff}
.muted{color:#57606a}
article.readme{border:1px solid #d0d7de;padding:8px 24px;margin-top:16px}
td.ln{color:#57606a;text-align:right;user-select:none;width:1%}
td.who{white-space:nowrap;width:1%}
form.act{display:inline-block;margin:12px 8px 0 0}
@media (prefers-color-scheme:dark){
body{color:#e6edf3;background:#0d1117}
header,pre{background:#161b22}
header,article.readme{border-color:#30363d}
td,th{border-color:#21262d}
a{color:#4493f8}
.add{background:#033a16}.del{background:#4b1113}.hunk{background:#0c2d6b;color:#9198a1}
.muted,td.ln{color:#9198a1}
}
";

/// Percent-encodes `s` for a URL path segment or query value.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn repo_url(repo: &str) -> String {
    format!("/{}", enc(repo))
}

fn tree_url(repo: &str, at: &str, path: Option<&str>) -> String {
    let path = path
        .map(|p| format!("&path={}", enc(p)))
        .unwrap_or_default();
    format!("/{}/tree?at={}{path}", enc(repo), enc(at))
}

fn log_url(repo: &str, at: &str, path: Option<&RepoPath>) -> String {
    let path = path
        .map(|p| format!("&path={}", enc(p.as_str())))
        .unwrap_or_default();
    format!("/{}/log?at={}{path}", enc(repo), enc(at))
}

fn blame_url(repo: &str, at: &str, path: &RepoPath) -> String {
    format!(
        "/{}/blame?at={}&path={}",
        enc(repo),
        enc(at),
        enc(path.as_str())
    )
}

fn changeset_url(repo: &str, id: ChangesetId) -> String {
    format!("/{}/changeset/{id}", enc(repo))
}

fn requests_url(repo: &str) -> String {
    format!("/{}/requests", enc(repo))
}

pub(crate) fn request_url(repo: &str, id: u64) -> String {
    format!("/{}/requests/{id}", enc(repo))
}

/// The first 12 hex digits of a changeset id.
fn short(id: ChangesetId) -> String {
    id.to_string()[..12].to_owned()
}

/// A message's first line.
fn summary(message: &str) -> &str {
    message.lines().next().unwrap_or("")
}

fn page(title: &str, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Scion" }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header { a href="/" { "Scion" } }
                main { (body) }
            }
        }
    }
}

/// The repository's name, its tabs, and the path below the root, each part
/// a link to its directory.
fn crumbs(repo: &str, at: &str, path: Option<&RepoPath>) -> Markup {
    html! {
        nav.tabs {
            a href=(repo_url(repo)) { "Code" }
            a href=(log_url(repo, at, None)) { "Log" }
            a href=(requests_url(repo)) { "Change requests" }
        }
        nav.crumbs {
            a href=(tree_url(repo, at, None)) { strong { (repo) } }
            @if let Some(path) = path {
                @let parts: Vec<&str> = path.as_str().split('/').collect();
                @for (i, part) in parts.iter().enumerate() {
                    " / "
                    @if i + 1 == parts.len() {
                        (part)
                    } @else {
                        a href=(tree_url(repo, at, Some(&parts[..=i].join("/")))) { (part) }
                    }
                }
            }
            span.muted { " at " span.mono { (at) } }
        }
    }
}

pub(crate) fn error(status: StatusCode, why: &str) -> Markup {
    page(
        status.canonical_reason().unwrap_or("Error"),
        &html! {
            h1 { (status.as_u16()) " " (status.canonical_reason().unwrap_or("")) }
            p { (why) }
        },
    )
}

/// The repositories `principal` reads, or the public ones for a visitor
/// who is not signed in; with a way to sign in or out.
pub(crate) fn repos(principal: Option<&str>, sign_in: bool, repos: &[String]) -> Markup {
    page(
        "Repositories",
        &html! {
            @match principal {
                Some(p) => p.muted {
                    "Signed in as " strong { (p) } ". "
                    form method="post" action="/logout" style="display:inline" {
                        button type="submit" { "Sign out" }
                    }
                },
                None if sign_in => p { a href="/login" { "Sign in with GitHub" } },
                None => p.muted {
                    "Showing public repositories. This server has no GitHub sign-in; send \
                     a token as Authorization: Bearer <token> to see yours."
                },
            }
            h1 { "Repositories" }
            @if repos.is_empty() {
                p.muted {
                    @match principal {
                        Some(p) => { (p) " holds no grant in any repository, and none is public." }
                        None => "No repository is public.",
                    }
                }
            } @else {
                ul {
                    @for repo in repos {
                        li { a href=(repo_url(repo)) { (repo) } }
                    }
                }
            }
        },
    )
}

/// Whether a top-level file is the README a repository home renders.
pub(crate) fn is_readme(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(lower.as_str(), "readme" | "readme.md" | "readme.txt")
}

pub(crate) fn home(
    repo: &str,
    refs: &[String],
    tree: &Tree,
    readme: Option<&(String, Content)>,
) -> Markup {
    page(
        repo,
        &html! {
            (crumbs(repo, crate::MAIN, None))
            p.muted {
                "Refs: "
                @for (i, name) in refs.iter().enumerate() {
                    @if i > 0 { ", " }
                    a.mono href=(tree_url(repo, name, None)) { (name) }
                }
            }
            (listing(repo, crate::MAIN, None, tree))
            @if let Some((name, content)) = readme {
                article.readme {
                    @match content {
                        Content::Text(text) if name.to_ascii_lowercase().ends_with(".md") => {
                            (PreEscaped(markdown(repo, text)))
                        }
                        Content::Text(text) => pre { (text) },
                        _ => p.muted { (name) " is not shown: binary or over 1 MiB." },
                    }
                }
            }
        },
    )
}

fn listing(repo: &str, at: &str, dir: Option<&RepoPath>, tree: &Tree) -> Markup {
    let child = |name: &str| match dir {
        Some(d) => format!("{d}/{name}"),
        None => name.to_owned(),
    };
    // Directories first, each group in name order.
    let dirs = tree
        .entries
        .iter()
        .filter(|(_, e)| matches!(e, TreeEntry::Tree { .. }));
    let files = tree.entries.iter().filter_map(|(n, e)| match e {
        TreeEntry::Blob { kind, size, .. } => Some((n, *kind, *size)),
        TreeEntry::Tree { .. } => None,
    });
    html! {
        table {
            @for (name, _) in dirs {
                tr {
                    td { a href=(tree_url(repo, at, Some(&child(name.as_str())))) { (name) "/" } }
                    td.muted { }
                }
            }
            @for (name, kind, size) in files {
                tr {
                    td {
                        a href=(tree_url(repo, at, Some(&child(name.as_str())))) { (name) }
                        @if kind == FileType::Executable { span.muted { " (executable)" } }
                        @if kind == FileType::Symlink { span.muted { " (symlink)" } }
                    }
                    td.muted { (size) " B" }
                }
            }
        }
    }
}

pub(crate) fn directory(repo: &str, at: &str, path: Option<&RepoPath>, tree: &Tree) -> Markup {
    page(
        path.map_or(repo, RepoPath::as_str),
        &html! {
            (crumbs(repo, at, path))
            @if let Some(p) = path {
                p { a href=(log_url(repo, at, Some(p))) { "History of " (p) "/" } }
            }
            (listing(repo, at, path, tree))
        },
    )
}

pub(crate) fn file(repo: &str, at: &str, path: &RepoPath, size: u64, content: &Content) -> Markup {
    page(
        path.as_str(),
        &html! {
            (crumbs(repo, at, Some(path)))
            p {
                span.muted { (size) " B · " }
                a href=(log_url(repo, at, Some(path))) { "History" }
                " · "
                a href=(blame_url(repo, at, path)) { "Blame" }
            }
            @match content {
                Content::Text(text) => {
                    table.mono {
                        @for (i, line) in text.lines().enumerate() {
                            tr { td.ln { (i + 1) } td { pre style="margin:0;padding:0;background:none" { (line) } } }
                        }
                    }
                }
                Content::Binary => p.muted { "Binary file, not shown." },
                Content::TooLarge => p.muted { "Over 1 MiB, not shown." },
            }
        },
    )
}

pub(crate) fn log(
    repo: &str,
    at: &str,
    path: Option<&RepoPath>,
    entries: &[Changeset],
    next: Option<ChangesetId>,
) -> Markup {
    page(
        &format!("Log of {}", path.map_or(repo, RepoPath::as_str)),
        &html! {
            (crumbs(repo, at, path))
            h2 { "Log" @if let Some(p) = path { " of " span.mono { (p) } } }
            table {
                @for cs in entries {
                    @let id = cs.id();
                    tr {
                        td.mono { a href=(changeset_url(repo, id)) { (short(id)) } }
                        td { (summary(&cs.message)) }
                        td.who { (cs.author.name) }
                        td.who.muted { (cs.author_date) }
                    }
                }
            }
            @if let Some(next) = next {
                p { a href=(log_url(repo, &next.to_string(), path)) { "Older" } }
            }
        },
    )
}

pub(crate) fn blame(
    repo: &str,
    at: &str,
    path: &RepoPath,
    runs: &[(ChangesetId, u64)],
    content: &Content,
) -> Markup {
    let text: Vec<&str> = match content {
        Content::Text(t) => t.lines().collect(),
        _ => Vec::new(),
    };
    let mut line = 0usize;
    html! {
        (page(&format!("Blame of {path}"), &html! {
            (crumbs(repo, at, Some(path)))
            h2 { "Blame of " span.mono { (path) } }
            table.mono {
                @for (id, lines) in runs {
                    @for i in 0..*lines {
                        @let n = { line += 1; line };
                        tr {
                            td.who {
                                @if i == 0 {
                                    a href=(changeset_url(repo, *id)) { (short(*id)) }
                                }
                            }
                            td.ln { (n) }
                            td {
                                pre style="margin:0;padding:0;background:none" {
                                    (text.get(n - 1).copied().unwrap_or(""))
                                }
                            }
                        }
                    }
                }
            }
        }))
    }
}

fn diff_path(entry: &DiffEntry) -> (&'static str, &RepoPath) {
    match entry {
        DiffEntry::Added { path, .. } => ("added", path),
        DiffEntry::Deleted { path, .. } => ("deleted", path),
        DiffEntry::Modified { path, .. } => ("modified", path),
    }
}

fn unified(patch: &str) -> Markup {
    html! {
        pre.diff {
            @for line in patch.lines() {
                @let class = match line.as_bytes().first() {
                    Some(b'+') => "add",
                    Some(b'-') => "del",
                    Some(b'@') => "hunk",
                    _ => "",
                };
                span class=(class) { (line) }
            }
        }
    }
}

pub(crate) fn changeset(
    repo: &str,
    id: ChangesetId,
    cs: &Changeset,
    entries: &[DiffEntry],
    diffs: &[Option<String>],
) -> Markup {
    let at = id.to_string();
    page(
        &format!("Changeset {}", short(id)),
        &html! {
            (crumbs(repo, &at, None))
            h2 { (summary(&cs.message)) }
            pre { (cs.message) }
            table {
                tr { th { "Changeset" } td.mono { (id) } }
                tr { th { "Author" } td { (cs.author.name) " <" (cs.author.email) "> · " (cs.author_date) } }
                @for parent in &cs.parents {
                    tr { th { "Parent" } td.mono { a href=(changeset_url(repo, *parent)) { (parent) } } }
                }
            }
            p { a href=(tree_url(repo, &at, None)) { "Browse files at this changeset" } }
            h3 { (entries.len()) " files changed" }
            @for (i, entry) in entries.iter().enumerate() {
                @let (what, path) = diff_path(entry);
                h4 {
                    span.mono {
                        @if what == "deleted" { (path) } @else { a href=(tree_url(repo, &at, Some(path.as_str()))) { (path) } }
                    }
                    span.muted { " " (what) }
                }
                @match diffs.get(i) {
                    Some(Some(hunks)) => (unified(hunks)),
                    Some(None) => p.muted { "Binary or over 1 MiB, not shown." },
                    None => {},
                }
            }
        },
    )
}

pub(crate) fn requests(repo: &str, requests: &[ChangeRequest]) -> Markup {
    page(
        &format!("Change requests of {repo}"),
        &html! {
            (crumbs(repo, crate::MAIN, None))
            h2 { "Change requests" }
            @if requests.is_empty() {
                p.muted { "No change requests yet." }
            }
            table {
                @for cr in requests {
                    tr {
                        td { a href=(request_url(repo, cr.id)) { "#" (cr.id) } }
                        td { (cr.status) }
                        td.mono { a href=(changeset_url(repo, cr.head)) { (short(cr.head)) } }
                        td { (cr.submitter) }
                        td.muted { (submitted(cr)) }
                    }
                }
            }
        },
    )
}

/// When a request was submitted, in UTC.
fn submitted(cr: &ChangeRequest) -> String {
    let secs = cr
        .submitted_at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    scion_types::Timestamp::new(i64::try_from(secs).unwrap_or(i64::MAX), 0).to_string()
}

pub(crate) fn request(
    repo: &str,
    cr: &ChangeRequest,
    actions: &[Action],
    outcome: Option<&Acted>,
) -> Markup {
    let conflicts = |paths: &[String]| html! { ul { @for path in paths { li.mono { (path) } } } };
    page(
        &format!("Change request #{}", cr.id),
        &html! {
            (crumbs(repo, crate::MAIN, None))
            h2 { "Change request #" (cr.id) " · " (cr.status) }
            table {
                tr { th { "Submitter" } td { (cr.submitter) } }
                tr { th { "Submitted" } td { (submitted(cr)) } }
                tr { th { "Head" } td.mono { a href=(changeset_url(repo, cr.head)) { (cr.head) } } }
                tr { th { "Base" } td.mono { a href=(changeset_url(repo, cr.base)) { (cr.base) } } }
                @if let Some(landed) = cr.landed {
                    tr { th { "Landed as" } td.mono { a href=(changeset_url(repo, landed)) { (landed) } } }
                }
            }
            @if !cr.conflicts.is_empty() {
                h3 {
                    @if cr.status == scion_store::ChangeRequestStatus::NeedsRebase {
                        "Conflicts with main"
                    } @else {
                        "Paths the submitter may not change"
                    }
                }
                (conflicts(&cr.conflicts))
            }
            @match outcome {
                Some(Acted::Rebased(id)) => {
                    h3 { "Rebased onto main" }
                    p {
                        "The head now replays cleanly onto main as "
                        a.mono href=(changeset_url(repo, *id)) { (short(*id)) }
                        "; land it to put it there."
                    }
                }
                Some(Acted::Conflicts(paths)) => {
                    h3 { "Still conflicts with main" }
                    (conflicts(paths))
                    p.muted {
                        "Resolve them in a working copy: " code { "scn update" } ", edit, "
                        code { "scn commit" } ", " code { "scn land" } "."
                    }
                }
                _ => {}
            }
            @for action in actions {
                form.act method="post" action=(format!("{}/{}", request_url(repo, cr.id), action.as_str())) {
                    button type="submit" {
                        @match action {
                            Action::Rebase => "Rebase onto main",
                            Action::Land => "Land",
                            Action::Reject => "Reject",
                        }
                    }
                }
            }
        },
    )
}

/// `src` as HTML. Raw HTML in it is shown as text, a link or image whose
/// scheme is not http, https or mailto goes nowhere, and a relative link
/// points at that path on `main`.
pub(crate) fn markdown(repo: &str, src: &str) -> String {
    let safe = |dest: CowStr<'_>| -> CowStr<'static> {
        let lower = dest.to_ascii_lowercase();
        if lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("mailto:")
            || dest.starts_with('#')
        {
            CowStr::from(dest.into_string())
        } else if dest.contains(':') || dest.starts_with('/') {
            CowStr::from("#")
        } else {
            let path = dest.split(['#', '?']).next().unwrap_or("");
            CowStr::from(tree_url(
                repo,
                crate::MAIN,
                Some(path.trim_start_matches("./")),
            ))
        }
    };
    let events = Parser::new_ext(
        src,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    )
    .map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            // Images load only over https (the page's policy); others show
            // their alt text.
            let dest_url = if dest_url.to_ascii_lowercase().starts_with("https://") {
                dest_url
            } else {
                CowStr::from("")
            };
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            })
        }
        e => e,
    });
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_shows_raw_html_as_text_and_drops_unsafe_links() {
        let out = markdown(
            "r",
            "# Hi\n\n<script>alert(1)</script>\n\n[x](javascript:alert(1)) \
             [y](https://example.com) [z](docs/a.md#top) ![i](http://e/i.png)",
        );
        assert!(out.contains("<h1>Hi</h1>"), "{out}");
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
        assert!(!out.contains("javascript:"), "{out}");
        assert!(out.contains(r#"href="https://example.com""#), "{out}");
        assert!(
            out.contains(r#"href="/r/tree?at=main&amp;path=docs%2Fa.md""#),
            "{out}"
        );
        assert!(!out.contains("http://e/i.png"), "{out}");
    }

    #[test]
    fn encodes_everything_but_unreserved_bytes() {
        assert_eq!(enc("release/1 a&b=ç"), "release%2F1%20a%26b%3D%C3%A7");
    }
}
