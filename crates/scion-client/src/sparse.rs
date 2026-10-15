//! Sparse profiles: which part of the repository a working copy materializes.
//!
//! A profile is a list of repository paths. A file is included when it is one
//! of them or lies below one; an empty profile includes everything. Checkout
//! only fetches the trees on the way to, and below, the listed paths.

use scion_types::{PathError, RepoPath};

/// Error parsing a sparse profile.
#[derive(Debug, thiserror::Error)]
#[error("sparse profile line {line}: {source}")]
pub struct SparseError {
    line: usize,
    #[source]
    source: PathError,
}

/// The paths a working copy materializes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SparseProfile {
    paths: Vec<RepoPath>,
}

/// Whether `path` is `prefix` or lies below it.
fn is_under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl SparseProfile {
    /// The profile that includes the whole repository.
    #[must_use]
    pub fn full() -> Self {
        Self::default()
    }

    /// A profile over `paths`. Duplicates and paths below another are kept;
    /// they do not change what is included.
    #[must_use]
    pub fn new(mut paths: Vec<RepoPath>) -> Self {
        paths.sort();
        paths.dedup();
        Self { paths }
    }

    /// The listed paths, sorted.
    #[must_use]
    pub fn paths(&self) -> &[RepoPath] {
        &self.paths
    }

    /// Parse the `.scn/sparse` format: one path per line; blank lines and
    /// lines starting with `#` are ignored.
    pub fn parse(text: &str) -> Result<Self, SparseError> {
        let mut paths = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let path = RepoPath::new(line).map_err(|source| SparseError {
                line: i + 1,
                source,
            })?;
            paths.push(path);
        }
        Ok(Self::new(paths))
    }

    /// The `.scn/sparse` format.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for p in &self.paths {
            out.push_str(p.as_str());
            out.push('\n');
        }
        out
    }

    /// Whether the file at `path` is materialized.
    #[must_use]
    pub fn includes(&self, path: &RepoPath) -> bool {
        self.paths.is_empty()
            || self
                .paths
                .iter()
                .any(|p| is_under(path.as_str(), p.as_str()))
    }

    /// Whether checkout must fetch the tree of directory `dir`: it is on the
    /// way to a listed path, or below one.
    #[must_use]
    pub fn descends_into(&self, dir: &RepoPath) -> bool {
        self.paths.is_empty()
            || self
                .paths
                .iter()
                .any(|p| is_under(dir.as_str(), p.as_str()) || is_under(p.as_str(), dir.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn full_profile_includes_everything() {
        let full = SparseProfile::full();
        assert!(full.includes(&p("a/b/c.txt")));
        assert!(full.descends_into(&p("a")));
    }

    #[test]
    fn includes_listed_paths_and_below() {
        let s = SparseProfile::parse("# services\nsrv/api\n\ntop.txt\n").unwrap();
        assert_eq!(s.paths(), &[p("srv/api"), p("top.txt")]);
        assert!(s.includes(&p("top.txt")));
        assert!(s.includes(&p("srv/api")));
        assert!(s.includes(&p("srv/api/main.rs")));
        assert!(
            !s.includes(&p("srv/apix/main.rs")),
            "prefix is per component"
        );
        assert!(!s.includes(&p("srv/other.rs")));

        assert!(s.descends_into(&p("srv")), "on the way to srv/api");
        assert!(s.descends_into(&p("srv/api/deep")));
        assert!(!s.descends_into(&p("srv/web")));
        assert!(!s.descends_into(&p("docs")));
    }

    #[test]
    fn round_trips_and_rejects_bad_paths() {
        let s = SparseProfile::new(vec![p("b"), p("a/x"), p("b")]);
        assert_eq!(SparseProfile::parse(&s.render()).unwrap(), s);
        let err = SparseProfile::parse("ok\n../escape\n").unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
    }
}
