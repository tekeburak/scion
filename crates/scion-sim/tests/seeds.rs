//! 200 seeds of the simulation, each checked against the invariants; a
//! failing seed panics with the command that replays it alone.
//! `SCION_SIM_SEED=<n>` runs only seed `n`.

use std::time::Instant;

#[test]
fn seeds_keep_the_invariants() {
    let seeds: Vec<u64> = match std::env::var("SCION_SIM_SEED") {
        Ok(seed) => vec![seed.parse().expect("SCION_SIM_SEED is a number")],
        Err(_) => (0..200).collect(),
    };
    let start = Instant::now();
    for seed in seeds {
        if let Err(why) = scion_sim::run(seed) {
            panic!("{}: seed {seed}: {why}", scion_sim::reproduce(seed));
        }
    }
    eprintln!("ran in {:?}", start.elapsed());
}

/// Seeds that landed a merge head's stack on `main` twice after a land
/// stopped between moving `main` and finishing the request (#148): 40 with
/// the store calls of the Phase 10.3 branch, the others on `main`.
#[test]
fn a_merge_head_lands_its_stack_once() {
    for seed in [40, 753, 1084, 1089, 1271, 1328] {
        if let Err(why) = scion_sim::run(seed) {
            panic!("{}: seed {seed}: {why}", scion_sim::reproduce(seed));
        }
    }
}

#[test]
fn a_seed_replays_the_same_history() {
    for seed in [7, 64, 199] {
        let first = scion_sim::run(seed).unwrap();
        assert!(
            first.requests.len() > 3,
            "seed {seed} lands too little to say much: {first:?}"
        );
        assert_eq!(scion_sim::run(seed).unwrap(), first, "seed {seed}");
    }
}
