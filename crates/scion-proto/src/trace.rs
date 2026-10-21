//! Per-stage timings for `SCION_TRACE=1`.
//!
//! With `SCION_TRACE=1` in its environment, every process that speaks the
//! protocol (`scn`, `scion-server`, `scion-ingest`) prints one line per
//! finished stage to stderr: `scion-trace <stage> <microseconds>`.
//! `scripts/bench-real` sums them per workload. Without it a span costs one
//! branch.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Instant;

/// A running stage; it prints its time when dropped.
#[derive(Debug)]
#[must_use = "a span measures until it is dropped"]
pub struct Span {
    stage: &'static str,
    start: Option<Instant>,
}

/// Start timing `stage`.
pub fn span(stage: &'static str) -> Span {
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| std::env::var_os("SCION_TRACE").is_some_and(|v| v == "1"));
    Span {
        stage,
        start: on.then(Instant::now),
    }
}

/// Await `f`, timed as `stage`.
pub async fn timed<F: Future>(stage: &'static str, f: F) -> F::Output {
    let _span = span(stage);
    f.await
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            eprintln!("scion-trace {} {}", self.stage, start.elapsed().as_micros());
        }
    }
}
