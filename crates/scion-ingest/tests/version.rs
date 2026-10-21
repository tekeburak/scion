//! `scion-ingest --version` needs no stores.

#[test]
fn version_is_the_workspace_version() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_scion-ingest"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let printed = String::from_utf8(out.stdout).unwrap();
    let want = concat!("scion-ingest ", env!("CARGO_PKG_VERSION"));
    assert!(printed.starts_with(want), "{printed}");
}
