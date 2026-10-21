//! Every input checked in under `fuzz/corpus/<target>/`, seeds and the
//! inputs of crashes fixed since, replayed through its target's check on
//! stable Rust. `cargo +nightly fuzz run <target>` explores further.

use std::path::Path;

#[test]
fn the_corpus_keeps_the_properties() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus");
    for (target, check) in scion_sim::fuzz::TARGETS {
        let mut inputs: Vec<_> = std::fs::read_dir(corpus.join(target))
            .unwrap_or_else(|e| panic!("{target}: {e}"))
            .map(|entry| entry.expect("a readable corpus").path())
            .collect();
        inputs.sort();
        assert!(!inputs.is_empty(), "{target} has no corpus");
        for input in inputs {
            eprintln!("{target}: {}", input.display());
            check(&std::fs::read(&input).expect("a readable input"));
        }
    }
}
