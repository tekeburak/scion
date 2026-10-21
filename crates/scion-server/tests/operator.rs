//! Operator commands: bootstrap access from an empty database.

use std::time::{Duration, SystemTime};

use scion_server::{Command, TokenKey, grant, revoke, token};
use scion_store::{Grant, MemoryStore, MetadataStore, Role, SERVER_GRANTS};

const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

/// Run one command line the way `scion-server` does, on `meta` and `KEY`.
async fn run(meta: &MemoryStore, line: &str, now: SystemTime) -> Result<String, String> {
    let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    match Command::parse(&args)? {
        Command::Grant { repo, grant: g } => grant(meta, &repo, &g).await,
        Command::Revoke {
            repo,
            principal,
            path_prefix,
        } => revoke(meta, &repo, &principal, &path_prefix).await,
        Command::Token { principal, ttl } => {
            token(&TokenKey::from_hex(KEY).unwrap(), &principal, ttl, now)
        }
        other => panic!("not an operator command: {other:?}"),
    }
}

fn admin(principal: &str) -> Grant {
    Grant {
        principal: principal.to_owned(),
        path_prefix: String::new(),
        role: Role::Admin,
    }
}

#[tokio::test]
async fn bootstraps_an_admin_and_a_service_principal_from_an_empty_database() {
    let meta = MemoryStore::new();
    let now = SystemTime::now();
    let key = TokenKey::from_hex(KEY).unwrap();

    assert_eq!(
        run(&meta, "grant * github:octocat admin", now).await,
        Ok("granted github:octocat admin on *".to_owned())
    );
    run(&meta, "grant * sp:ci admin", now).await.unwrap();
    let admin_token = run(&meta, "token github:octocat --ttl 1h", now)
        .await
        .unwrap();
    let ci_token = run(&meta, "token sp:ci --ttl 30d", now).await.unwrap();

    assert_eq!(
        key.verify(&admin_token, now + Duration::from_secs(3_599))
            .map(|(p, _)| p),
        Ok("github:octocat".to_owned())
    );
    assert!(
        key.verify(&admin_token, now + Duration::from_secs(3_600))
            .is_err()
    );
    assert_eq!(
        key.verify(&ci_token, now + Duration::from_secs(29 * 86_400))
            .map(|(p, _)| p),
        Ok("sp:ci".to_owned())
    );
    assert_eq!(
        meta.grants_for(SERVER_GRANTS, "github:octocat")
            .await
            .unwrap(),
        [admin("github:octocat")]
    );
    assert_eq!(
        meta.grants_for(SERVER_GRANTS, "sp:ci").await.unwrap(),
        [admin("sp:ci")]
    );

    // Revoking takes the grant away; the token stays valid until it expires.
    assert_eq!(
        run(&meta, "revoke * sp:ci", now).await,
        Ok("revoked sp:ci on *".to_owned())
    );
    assert_eq!(
        meta.grants_for(SERVER_GRANTS, "sp:ci").await.unwrap(),
        Vec::<Grant>::new()
    );
    let err = run(&meta, "revoke * sp:ci", now).await.unwrap_err();
    assert_eq!(err, "sp:ci has no grant on *");
}

#[tokio::test]
async fn grants_a_directory_of_an_existing_repository() {
    let meta = MemoryStore::new();
    let now = SystemTime::now();
    let err = run(&meta, "grant web sp:ci writer", now).await.unwrap_err();
    assert!(err.contains("scn init"), "{err}");

    meta.create_repo("web").await.unwrap();
    assert_eq!(
        run(&meta, "grant web sp:ci writer --path lib/", now).await,
        Ok("granted sp:ci writer on web:lib/".to_owned())
    );
    run(&meta, "grant web sp:ci reader", now).await.unwrap();
    assert_eq!(
        meta.grants_for("web", "sp:ci").await.unwrap(),
        [
            Grant {
                principal: "sp:ci".to_owned(),
                path_prefix: String::new(),
                role: Role::Reader,
            },
            Grant {
                principal: "sp:ci".to_owned(),
                path_prefix: "lib".to_owned(),
                role: Role::Writer,
            },
        ]
    );
    assert_eq!(
        meta.grants_for(SERVER_GRANTS, "sp:ci").await.unwrap(),
        Vec::<Grant>::new()
    );

    assert_eq!(
        run(&meta, "revoke web sp:ci --path lib", now).await,
        Ok("revoked sp:ci on web:lib/".to_owned())
    );
    assert_eq!(meta.grants_for("web", "sp:ci").await.unwrap().len(), 1);
}
