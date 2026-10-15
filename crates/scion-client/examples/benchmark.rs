//! Scion against git on a synthetic repository.
//!
//! ```sh
//! cargo run --release -p scion-client --example benchmark -- [files-per-dir=100] [runs=5]
//! ```
//!
//! The fixture is 10 directories of `files-per-dir` files, 4 KiB of seeded
//! pseudorandom bytes each. Every run starts a fresh in-process `scion-server`
//! over loopback gRPC with the in-memory store (not Postgres + S3), and fresh
//! git repositories. Four workloads are timed:
//!
//! * import: git `init` + `add -A` + `commit`; Scion uploads every blob, then
//!   `CreateChangeset` and `MoveRef` to create `main`;
//! * commit one changed file: git `commit -a`; Scion `PutBlob` +
//!   `CreateChangeset` + `MoveRef` (there is no `scn commit` yet, so no
//!   working-copy scan is timed on the Scion side);
//! * full clone: `git clone --no-local` against `scion_client::clone`;
//! * one-directory clone: git partial clone + `sparse-checkout set dir0000`
//!   against `scion_client::clone` with a one-directory sparse profile.
//!
//! Correctness is checked outside the timers. Medians in milliseconds go to
//! stdout as a Markdown table; per-run samples go to stderr.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use scion_client::{Remote, SparseProfile, clone};
use scion_proto::v1::{CreateChangesetRequest, MoveRefRequest, PutBlobRequest};
use scion_store::MemoryStore;
use scion_types::{
    BlobId, Changeset, ChangesetId, Encode, FileChange, FileType, Identity, RepoPath, Timestamp,
};
use tokio::net::TcpListener;

const REPO: &str = "bench";
const DIRS: usize = 10;
const FILE_BYTES: usize = 4096;
const UPLOAD_CONCURRENCY: usize = 64;
const SCENARIOS: [&str; 4] = [
    "Import directory",
    "Commit one changed file",
    "Full clone / checkout",
    "One-directory clone / checkout",
];

type Files = Vec<(String, Vec<u8>)>;

/// Deterministic file contents (xorshift64*), so no RNG dependency is needed.
fn fixture(files_per_dir: usize) -> Files {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut files = Vec::with_capacity(DIRS * files_per_dir);
    for d in 0..DIRS {
        for f in 0..files_per_dir {
            let data: Vec<u8> = (0..FILE_BYTES / 8)
                .flat_map(|_| next().to_le_bytes())
                .collect();
            files.push((format!("dir{d:04}/file{f:05}.bin"), data));
        }
    }
    files
}

fn write_fixture(root: &Path, files: &Files) {
    for (path, data) in files {
        let dest = root.join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, data).unwrap();
    }
}

/// Every fixture file is present byte for byte; with `sparse`, only `dir0000/`.
fn verify(root: &Path, files: &Files, sparse: bool) {
    for (path, data) in files {
        let on_disk = std::fs::read(root.join(path)).ok();
        if sparse && !path.starts_with("dir0000/") {
            assert!(on_disk.is_none(), "{path} outside the sparse profile");
        } else {
            assert_eq!(on_disk.as_deref(), Some(&data[..]), "{path} differs");
        }
    }
}

/// Run git with no user or system configuration and no hooks.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=bench",
            "-c",
            "user.email=bench@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "uploadpack.allowFilter=true",
        ])
        .args(args)
        .status()
        .expect("git must be on PATH");
    assert!(status.success(), "git {args:?} failed");
}

fn tracked(data: &[u8]) -> FileChange {
    FileChange::Tracked {
        content: BlobId::hash(data),
        kind: FileType::Regular,
        size: data.len() as u64,
        copy_from: None,
    }
}

fn changeset(parents: Vec<ChangesetId>, file_changes: BTreeMap<RepoPath, FileChange>) -> Changeset {
    Changeset {
        repo: REPO.into(),
        parents,
        author: Identity::new("bench", "bench@example.com"),
        author_date: Timestamp::new(0, 0),
        committer: None,
        committer_date: None,
        message: "bench".into(),
        file_changes,
        extra: BTreeMap::new(),
    }
}

async fn put_blob(remote: &Remote, data: Vec<u8>) {
    remote
        .objects
        .clone()
        .put_blob(PutBlobRequest { data })
        .await
        .unwrap();
}

async fn create_changeset(remote: &Remote, cs: &Changeset) -> ChangesetId {
    let resp = remote
        .control
        .clone()
        .create_changeset(CreateChangesetRequest {
            changeset: cs.encode_to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    ChangesetId::try_from_slice(&resp.id).unwrap()
}

async fn move_main(remote: &Remote, target: ChangesetId, expected_version: Option<u64>) -> u64 {
    remote
        .control
        .clone()
        .move_ref(MoveRefRequest {
            repo: REPO.into(),
            name: "main".into(),
            target: target.as_bytes().to_vec(),
            expected_version,
        })
        .await
        .unwrap()
        .into_inner()
        .version
}

/// Read the files from `root`, upload every blob, create the root changeset
/// and point `main` at it.
async fn import(remote: &Remote, root: &Path, files: &Files) -> (ChangesetId, u64) {
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    let changes: BTreeMap<RepoPath, FileChange> = stream::iter(paths.into_iter().map(|path| {
        let remote = remote.clone();
        async move {
            let data = tokio::fs::read(root.join(path)).await.unwrap();
            let change = tracked(&data);
            put_blob(&remote, data).await;
            (RepoPath::new(path).unwrap(), change)
        }
    }))
    .buffer_unordered(UPLOAD_CONCURRENCY)
    .collect()
    .await;
    let id = create_changeset(remote, &changeset(vec![], changes)).await;
    (id, move_main(remote, id, None).await)
}

async fn serve() -> String {
    let store = Arc::new(MemoryStore::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(scion_server::serve(listener, store.clone(), store));
    format!("http://{addr}")
}

fn timed<T>(samples: &mut Vec<Duration>, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    samples.push(start.elapsed());
    out
}

async fn timed_async<T>(samples: &mut Vec<Duration>, f: impl Future<Output = T>) -> T {
    let start = Instant::now();
    let out = f.await;
    samples.push(start.elapsed());
    out
}

fn median_ms(samples: &mut [Duration]) -> f64 {
    samples.sort();
    let n = samples.len();
    let mid = if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2
    };
    mid.as_secs_f64() * 1000.0
}

/// One run of all four workloads; pushes one sample per workload to each side.
async fn run_once(files: &Files, git_s: &mut [Vec<Duration>; 4], scn_s: &mut [Vec<Duration>; 4]) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_fixture(&src, files);
    let server = serve().await;
    let remote = Remote::connect(&server).await.unwrap();

    // Import.
    timed(&mut git_s[0], || {
        git(&src, &["init", "-q", "-b", "main"]);
        git(&src, &["add", "-A"]);
        git(&src, &["commit", "-q", "-m", "import"]);
    });
    let (root, version) = timed_async(&mut scn_s[0], import(&remote, &src, files)).await;

    // Commit one changed file: flip one byte of the first file.
    let (path, data) = &files[0];
    let mut changed = data.clone();
    changed[0] ^= 0xff;
    std::fs::write(src.join(path), &changed).unwrap();
    timed(&mut git_s[1], || {
        git(&src, &["commit", "-q", "-a", "-m", "one"]);
    });
    timed_async(&mut scn_s[1], async {
        let change = tracked(&changed);
        put_blob(&remote, changed.clone()).await;
        let cs = changeset(
            vec![root],
            BTreeMap::from([(RepoPath::new(path.as_str()).unwrap(), change)]),
        );
        let id = create_changeset(&remote, &cs).await;
        move_main(&remote, id, Some(version)).await;
    })
    .await;
    let mut expected = files.clone();
    expected[0].1 = changed;

    // Full clone.
    let git_full = tmp.path().join("git-full");
    let src_url = src.to_str().unwrap();
    timed(&mut git_s[2], || {
        git(
            tmp.path(),
            &["clone", "-q", "--no-local", src_url, "git-full"],
        );
    });
    verify(&git_full, &expected, false);
    let scn_full = tmp.path().join("scn-full");
    timed_async(
        &mut scn_s[2],
        clone(&server, REPO, &scn_full, SparseProfile::full()),
    )
    .await
    .unwrap();
    verify(&scn_full, &expected, false);

    // One-directory clone.
    let git_sparse = tmp.path().join("git-sparse");
    let file_url = format!("file://{src_url}");
    timed(&mut git_s[3], || {
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--no-local",
                "--filter=blob:none",
                "--sparse",
                &file_url,
                "git-sparse",
            ],
        );
        git(&git_sparse, &["sparse-checkout", "set", "dir0000"]);
    });
    verify(&git_sparse, &expected, true);
    let scn_sparse = tmp.path().join("scn-sparse");
    timed_async(
        &mut scn_s[3],
        clone(
            &server,
            REPO,
            &scn_sparse,
            SparseProfile::new(vec![RepoPath::new("dir0000").unwrap()]),
        ),
    )
    .await
    .unwrap();
    verify(&scn_sparse, &expected, true);
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1).map(|a| {
        a.parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .expect("usage: benchmark [files-per-dir] [runs], both positive integers")
    });
    let files_per_dir = args.next().unwrap_or(100);
    let runs = args.next().unwrap_or(5);
    let files = fixture(files_per_dir);

    let mut git_s: [Vec<Duration>; 4] = Default::default();
    let mut scn_s: [Vec<Duration>; 4] = Default::default();

    for run in 0..runs {
        run_once(&files, &mut git_s, &mut scn_s).await;
        for (i, name) in SCENARIOS.iter().enumerate() {
            eprintln!(
                "run {run}: {name}: git {:.1} ms, scion {:.1} ms",
                git_s[i][run].as_secs_f64() * 1000.0,
                scn_s[i][run].as_secs_f64() * 1000.0,
            );
        }
    }

    println!(
        "{} files ({DIRS} directories × {files_per_dir}), {runs} runs, median ms\n",
        DIRS * files_per_dir
    );
    println!("| Scenario | Git (ms) | Scion (ms) |\n| --- | ---: | ---: |");
    for (i, name) in SCENARIOS.iter().enumerate() {
        println!(
            "| {name} | {:.1} | {:.1} |",
            median_ms(&mut git_s[i]),
            median_ms(&mut scn_s[i])
        );
    }
}
