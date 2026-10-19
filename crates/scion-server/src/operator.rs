//! The operator's command line: `scion-server` subcommands that work on the
//! stores directly, without a running server.

use std::time::{Duration, SystemTime};

use scion_store::{Grant, MetadataStore, Role, SERVER_GRANTS};
use scion_types::RepoPath;

use crate::token::{TokenKey, check_principal};

/// Usage of every `scion-server` command.
pub const USAGE: &str = "\
usage: scion-server                                          serve
       scion-server backfill <repo> <type>
       scion-server grant <repo|*> <principal> <role> [--path <dir>]
       scion-server revoke <repo|*> <principal> [--path <dir>]
       scion-server token <principal> --ttl <duration>";

/// A parsed `scion-server` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Serve the gRPC services.
    Serve,
    /// Derive `ty` for `repo`'s whole history.
    Backfill { repo: String, ty: String },
    /// Give a principal a role in a repository, or server-wide under `*`.
    Grant { repo: String, grant: Grant },
    /// Remove a principal's grant on a path prefix.
    Revoke {
        repo: String,
        principal: String,
        path_prefix: String,
    },
    /// Issue a token for a principal, valid for `ttl`.
    Token { principal: String, ttl: Duration },
}

impl Command {
    /// Parse the arguments after the program name. Principals, roles, paths
    /// and durations are checked here, at the boundary.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] => Ok(Self::Serve),
            ["backfill", repo, ty] => Ok(Self::Backfill {
                repo: (*repo).to_owned(),
                ty: (*ty).to_owned(),
            }),
            ["grant", repo, principal, role, rest @ ..] => {
                let role = Role::parse(role).ok_or_else(|| {
                    format!("unknown role {role:?}; roles are reader, writer, admin")
                })?;
                let path_prefix = path_option(repo, rest)?;
                if *repo == SERVER_GRANTS && role != Role::Admin {
                    return Err(format!(
                        "a server-wide grant ({SERVER_GRANTS}) is admin; got {role}"
                    ));
                }
                Ok(Self::Grant {
                    repo: (*repo).to_owned(),
                    grant: Grant {
                        principal: principal_arg(principal)?,
                        path_prefix,
                        role,
                    },
                })
            }
            ["revoke", repo, principal, rest @ ..] => Ok(Self::Revoke {
                repo: (*repo).to_owned(),
                principal: principal_arg(principal)?,
                path_prefix: path_option(repo, rest)?,
            }),
            ["token", principal, "--ttl", ttl] => Ok(Self::Token {
                principal: principal_arg(principal)?,
                ttl: parse_ttl(ttl)?,
            }),
            _ => Err(USAGE.to_owned()),
        }
    }
}

/// Record `grant` in `repo` and say what was granted.
pub async fn grant(meta: &dyn MetadataStore, repo: &str, grant: &Grant) -> Result<String, String> {
    meta.put_grant(repo, grant)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "granted {} {} on {}",
        grant.principal,
        grant.role,
        scope(repo, &grant.path_prefix)
    ))
}

/// Remove a grant and say whether there was one.
pub async fn revoke(
    meta: &dyn MetadataStore,
    repo: &str,
    principal: &str,
    path_prefix: &str,
) -> Result<String, String> {
    let removed = meta
        .delete_grant(repo, principal, path_prefix)
        .await
        .map_err(|e| e.to_string())?;
    let at = scope(repo, path_prefix);
    if removed {
        Ok(format!("revoked {principal} on {at}"))
    } else {
        Err(format!("{principal} has no grant on {at}"))
    }
}

/// Issue a token for `principal` that expires `ttl` after `now`.
pub fn token(
    key: &TokenKey,
    principal: &str,
    ttl: Duration,
    now: SystemTime,
) -> Result<String, String> {
    key.issue(principal, now + ttl).map_err(|e| e.to_string())
}

fn scope(repo: &str, path_prefix: &str) -> String {
    if path_prefix.is_empty() {
        repo.to_owned()
    } else {
        format!("{repo}:{path_prefix}/")
    }
}

fn principal_arg(principal: &str) -> Result<String, String> {
    check_principal(principal).map_err(|e| e.to_string())?;
    Ok(principal.to_owned())
}

/// The prefix `[--path <dir>]` names: a repository directory without the
/// trailing `/`, or empty for the whole repository.
fn path_option(repo: &str, rest: &[&str]) -> Result<String, String> {
    match rest {
        [] => Ok(String::new()),
        ["--path", dir] => {
            if repo == SERVER_GRANTS {
                return Err(format!(
                    "a server-wide grant ({SERVER_GRANTS}) has no --path"
                ));
            }
            let dir = dir.trim_end_matches('/');
            RepoPath::new(dir).map_err(|e| format!("--path {dir:?}: {e}"))?;
            Ok(dir.to_owned())
        }
        _ => Err(USAGE.to_owned()),
    }
}

/// A duration such as `90s`, `30m`, `12h` or `30d`.
fn parse_ttl(s: &str) -> Result<Duration, String> {
    let bad = || format!("--ttl {s:?}: expected a number followed by s, m, h or d, as in 30d");
    let unit = match s.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 60 * 60,
        Some('d') => 24 * 60 * 60,
        _ => return Err(bad()),
    };
    let n: u64 = s[..s.len() - 1].parse().map_err(|_| bad())?;
    match n.checked_mul(unit) {
        Some(secs) if secs > 0 => Ok(Duration::from_secs(secs)),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Command, String> {
        let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        Command::parse(&args)
    }

    #[test]
    fn ttls() {
        assert_eq!(parse_ttl("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_ttl("30m"), Ok(Duration::from_secs(1_800)));
        assert_eq!(parse_ttl("12h"), Ok(Duration::from_secs(43_200)));
        assert_eq!(parse_ttl("30d"), Ok(Duration::from_secs(2_592_000)));
        for bad in [
            "",
            "d",
            "0d",
            "30",
            "1w",
            "-1d",
            "1.5h",
            "99999999999999999999d",
        ] {
            assert!(parse_ttl(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn paths_drop_the_trailing_slash() {
        let Ok(Command::Grant { grant, .. }) = parse("grant web sp:ci writer --path lib/net/")
        else {
            panic!()
        };
        assert_eq!(grant.path_prefix, "lib/net");
    }

    #[test]
    fn bad_command_lines_are_refused() {
        for (line, says) in [
            ("grant web sp:ci owner", "unknown role"),
            ("grant web alice writer", "github:<login> or sp:<name>"),
            ("grant * sp:ci writer", "server-wide grant"),
            ("grant * sp:ci admin --path lib", "has no --path"),
            ("grant web sp:ci writer --path /lib", "--path"),
            ("grant web sp:ci writer --path ../x", "--path"),
            ("grant web sp:ci writer lib", "usage:"),
            ("revoke web sp:ci --path", "usage:"),
            ("token sp:ci", "usage:"),
            ("token sp:ci --ttl soon", "--ttl"),
            ("serve", "usage:"),
        ] {
            let err = parse(line).unwrap_err();
            assert!(err.contains(says), "{line}: {err}");
        }
    }
}
