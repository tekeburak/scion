//! `SkipList`: the ancestry index.
//!
//! Every changeset stores its generation (`max(parent generation) + 1`, roots
//! are 0), its parents, and — for single-parent changesets only — *skip*
//! pointers to the ancestors at first-parent distance 1, 2, 4, 8, … (binary
//! lifting). A merge has no skips, so ancestry queries expand its parents.
//!
//! The core query is `frontier_at(x, g)`: the set of ancestors-or-self of `x`
//! whose generation is exactly `g`. Along a run of single-parent changesets
//! the set of ancestors below any generation is the same for every changeset
//! in the run, so the walk may follow the longest skip that does not pass
//! below `g`; only merges branch. Everything else is built on it:
//!
//! * `is_ancestor(a, b)` ⇔ `a ∈ frontier_at(b, gen(a))`.
//! * `merge_base(a, b)`: for every `g ≤ gen(merge base)` there is a common
//!   ancestor at exactly `g` (generations decrease by one per edge), so
//!   "`frontier_at(a, g) ∩ frontier_at(b, g) ≠ ∅`" is monotone in `g` and the
//!   highest such `g` is found by binary search: O(log² n) lookups instead of
//!   a walk over the history between the two heads.

use std::collections::{BTreeSet, HashSet};

use async_trait::async_trait;
use scion_types::canonical::{Reader, Writer};
use scion_types::{Changeset, ChangesetId};

use crate::{Derivable, Deriver, Result};

/// Ancestry index entry of one changeset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipList {
    pub generation: u64,
    /// `(parent, generation of parent)` in changeset order.
    pub parents: Vec<(ChangesetId, u64)>,
    /// `skips[k]` is the first-parent ancestor at distance `2^k`, with its
    /// generation. Empty unless the changeset has exactly one parent.
    pub skips: Vec<(ChangesetId, u64)>,
}

#[async_trait]
impl Derivable for SkipList {
    const NAME: &'static str = "skip_list";
    const VERSION: u32 = 1;

    async fn derive(ctx: &Deriver<'_>, cs: &Changeset, parents: &[Self]) -> Result<Self> {
        let generation = parents.iter().map(|p| p.generation + 1).max().unwrap_or(0);
        let parents: Vec<(ChangesetId, u64)> = cs
            .parents
            .iter()
            .zip(parents)
            .map(|(id, p)| (*id, p.generation))
            .collect();

        let mut skips = Vec::new();
        if let [parent] = parents.as_slice() {
            // skips[k] = skips[k-1] of the changeset at skips[k-1]; every
            // target is an ancestor, so it is already derived and stored.
            skips.push(*parent);
            loop {
                let k = skips.len();
                let (target, _) = skips[k - 1];
                let Some(next) = ctx
                    .derive::<SkipList>(target)
                    .await?
                    .skips
                    .get(k - 1)
                    .copied()
                else {
                    break;
                };
                skips.push(next);
            }
        }
        Ok(Self {
            generation,
            parents,
            skips,
        })
    }

    fn to_pointer(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u64(self.generation);
        for list in [&self.parents, &self.skips] {
            w.seq(list.iter(), |w, (id, generation)| {
                w.id(id.as_bytes());
                w.u64(*generation);
            });
        }
        w.into_vec()
    }

    fn from_pointer(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(bytes);
        let generation = r.u64().ok()?;
        let mut lists = [Vec::new(), Vec::new()];
        for list in &mut lists {
            let n = r.seq_len().ok()?;
            for _ in 0..n {
                let id = ChangesetId::from_bytes(r.id().ok()?);
                let generation = r.u64().ok()?;
                list.push((id, generation));
            }
        }
        r.finish().ok()?;
        let [parents, skips] = lists;
        Some(Self {
            generation,
            parents,
            skips,
        })
    }
}

impl SkipList {
    /// Ancestors-or-self of `x` whose generation is exactly `g`.
    pub async fn frontier_at(
        ctx: &Deriver<'_>,
        x: ChangesetId,
        g: u64,
    ) -> Result<BTreeSet<ChangesetId>> {
        let mut found = BTreeSet::new();
        let mut visited = HashSet::new();
        let mut stack = vec![x];
        while let Some(node) = stack.pop() {
            if !visited.insert(node) {
                continue;
            }
            let entry = ctx.derive::<SkipList>(node).await?;
            if entry.generation == g {
                found.insert(node);
                continue;
            }
            if entry.generation < g {
                continue;
            }
            // Longest skip that stays at or above g; otherwise all parents at
            // or above g (a merge, or a run whose skips all overshoot).
            match entry
                .skips
                .iter()
                .rev()
                .find(|(_, generation)| *generation >= g)
            {
                Some((target, _)) => stack.push(*target),
                None => stack.extend(
                    entry
                        .parents
                        .iter()
                        .filter(|(_, generation)| *generation >= g)
                        .map(|(id, _)| *id),
                ),
            }
        }
        Ok(found)
    }

    /// Whether `ancestor` is an ancestor of, or equal to, `descendant`.
    pub async fn is_ancestor(
        ctx: &Deriver<'_>,
        ancestor: ChangesetId,
        descendant: ChangesetId,
    ) -> Result<bool> {
        let g = ctx.derive::<SkipList>(ancestor).await?.generation;
        Ok(Self::frontier_at(ctx, descendant, g)
            .await?
            .contains(&ancestor))
    }

    /// The common ancestor of `a` and `b` with the highest generation, or
    /// `None` if they share no ancestor. Among several common ancestors of
    /// that generation (criss-cross merges) the smallest id is returned, so
    /// the result is deterministic.
    pub async fn merge_base(
        ctx: &Deriver<'_>,
        a: ChangesetId,
        b: ChangesetId,
    ) -> Result<Option<ChangesetId>> {
        let gen_a = ctx.derive::<SkipList>(a).await?.generation;
        let gen_b = ctx.derive::<SkipList>(b).await?.generation;

        // Binary search for the highest g with a common ancestor at exactly g.
        // Invariant: `best` holds the intersection at the highest g known to
        // have one; `lo..=hi` is the unexplored range.
        let mut best: Option<BTreeSet<ChangesetId>> = None;
        let (mut lo, mut hi) = (0u64, gen_a.min(gen_b));
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let fa = Self::frontier_at(ctx, a, mid).await?;
            let fb = Self::frontier_at(ctx, b, mid).await?;
            let common: BTreeSet<ChangesetId> = fa.intersection(&fb).copied().collect();
            if common.is_empty() {
                if mid == 0 {
                    break;
                }
                hi = mid - 1;
            } else {
                best = Some(common);
                lo = mid + 1;
            }
        }
        Ok(best.and_then(|set| set.into_iter().next()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};

    use proptest::prelude::*;
    use scion_store::{MemoryStore, MetadataStore};

    use super::*;
    use crate::test_util::{CountingMeta, REPO, changeset};

    // ---- naive reference ------------------------------------------------

    /// All ancestors-or-self of `x`, by walking parents.
    async fn ancestors(meta: &dyn MetadataStore, x: ChangesetId) -> HashSet<ChangesetId> {
        let mut seen = HashSet::new();
        let mut stack = vec![x];
        while let Some(n) = stack.pop() {
            if seen.insert(n) {
                let cs = meta.get_changeset(REPO, n).await.unwrap().unwrap();
                stack.extend(cs.parents);
            }
        }
        seen
    }

    async fn naive_merge_base(
        meta: &dyn MetadataStore,
        a: ChangesetId,
        b: ChangesetId,
    ) -> Option<ChangesetId> {
        let common: Vec<ChangesetId> = ancestors(meta, a)
            .await
            .intersection(&ancestors(meta, b).await)
            .copied()
            .collect();
        let mut best: Option<(u64, ChangesetId)> = None;
        for c in common {
            let g = meta.generation(REPO, c).await.unwrap().unwrap();
            // Highest generation, then smallest id.
            if best.is_none_or(|(bg, bid)| g > bg || (g == bg && c < bid)) {
                best = Some((g, c));
            }
        }
        best.map(|(_, id)| id)
    }

    // ---- builders -------------------------------------------------------

    /// Store a changeset with the given parents; the message makes it unique.
    async fn node(store: &MemoryStore, parents: Vec<ChangesetId>, tag: &str) -> ChangesetId {
        store
            .put_changeset(&changeset(parents, BTreeMap::new(), tag))
            .await
            .unwrap()
    }

    /// A linear chain of `n` changesets; returns them root-first.
    async fn chain(store: &MemoryStore, n: usize, tag: &str) -> Vec<ChangesetId> {
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let parents = ids.last().copied().into_iter().collect();
            ids.push(node(store, parents, &format!("{tag}{i}")).await);
        }
        ids
    }

    // ---- tests ----------------------------------------------------------

    #[test]
    fn pointer_roundtrips() {
        let v = SkipList {
            generation: 7,
            parents: vec![(ChangesetId::from_bytes([1; 32]), 6)],
            skips: vec![
                (ChangesetId::from_bytes([1; 32]), 6),
                (ChangesetId::from_bytes([2; 32]), 5),
            ],
        };
        assert_eq!(SkipList::from_pointer(&v.to_pointer()), Some(v));
        assert_eq!(SkipList::from_pointer(b"x"), None);
    }

    #[tokio::test]
    async fn skips_point_at_powers_of_two() {
        let store = MemoryStore::with_repo(REPO);
        let ids = chain(&store, 40, "c").await;
        let d = Deriver::new(REPO, &store, &store);
        let top = d.derive::<SkipList>(ids[39]).await.unwrap();
        assert_eq!(top.generation, 39);
        assert_eq!(top.parents, vec![(ids[38], 38)]);
        let expected: Vec<_> = [1usize, 2, 4, 8, 16, 32]
            .into_iter()
            .map(|dist| (ids[39 - dist], 39 - dist as u64))
            .collect();
        assert_eq!(top.skips, expected);
        // A merge has no skips.
        let merge = node(&store, vec![ids[39], ids[10]], "m").await;
        let m = d.derive::<SkipList>(merge).await.unwrap();
        assert_eq!(m.generation, 40);
        assert_eq!(m.skips.len(), 0);
        assert_eq!(m.parents.len(), 2);
    }

    #[tokio::test]
    async fn ancestry_reads_are_logarithmic_on_a_long_chain() {
        let store = MemoryStore::with_repo(REPO);
        let ids = chain(&store, 1000, "c").await;
        // Build the index up front, then count what the queries read.
        Deriver::new(REPO, &store, &store)
            .derive::<SkipList>(ids[999])
            .await
            .unwrap();

        let counting = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &counting, &store);
        assert!(SkipList::is_ancestor(&d, ids[3], ids[999]).await.unwrap());
        let reads = counting.derived_reads();
        assert!(
            reads <= 40,
            "is_ancestor read {reads} index entries on a 1000-chain"
        );

        let counting = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &counting, &store);
        assert!(!SkipList::is_ancestor(&d, ids[999], ids[3]).await.unwrap());
        assert!(counting.derived_reads() <= 5);

        // Two branches off an old point of a long chain.
        let left = node(&store, vec![ids[500]], "l").await;
        let right = node(&store, vec![ids[500]], "r").await;
        let counting = CountingMeta::new(&store);
        let d = Deriver::new(REPO, &counting, &store);
        assert_eq!(
            SkipList::merge_base(&d, left, ids[999]).await.unwrap(),
            Some(ids[500])
        );
        assert_eq!(
            SkipList::merge_base(&d, left, right).await.unwrap(),
            Some(ids[500])
        );
        let reads = counting.derived_reads();
        assert!(
            reads <= 600,
            "merge_base read {reads} index entries; a walk would read ~1000"
        );
        assert_eq!(
            counting.changeset_reads(),
            2,
            "only the two new changesets were underived"
        );
    }

    #[tokio::test]
    async fn unrelated_roots_have_no_merge_base() {
        let store = MemoryStore::with_repo(REPO);
        let a = chain(&store, 3, "a").await;
        let b = chain(&store, 3, "b").await;
        let d = Deriver::new(REPO, &store, &store);
        assert_eq!(SkipList::merge_base(&d, a[2], b[2]).await.unwrap(), None);
        assert!(!SkipList::is_ancestor(&d, a[0], b[2]).await.unwrap());
        assert!(SkipList::is_ancestor(&d, a[2], a[2]).await.unwrap());
    }

    // ---- property test: random DAGs ≡ naive BFS ------------------------

    /// A DAG as parent indices: node i may only point at nodes < i.
    fn arb_dag() -> impl Strategy<Value = Vec<Vec<usize>>> {
        (1usize..24).prop_flat_map(|n| {
            (0..n)
                .map(|i| {
                    if i == 0 {
                        Just(vec![]).boxed()
                    } else {
                        prop_oneof![
                            1 => Just(vec![]),                              // another root
                            6 => (0..i).prop_map(|p| vec![p]),              // single parent
                            3 => (0..i, 0..i).prop_map(|(p, q)| if p == q { vec![p] } else { vec![p, q] }),
                        ]
                        .boxed()
                    }
                })
                .collect::<Vec<_>>()
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]
        #[test]
        fn queries_equal_naive(dag in arb_dag(), probes in prop::collection::vec((any::<prop::sample::Index>(), any::<prop::sample::Index>()), 1..6)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = MemoryStore::with_repo(REPO);
                let d = Deriver::new(REPO, &store, &store);
                let mut ids: Vec<ChangesetId> = Vec::new();
                for (i, parents) in dag.iter().enumerate() {
                    let parents = parents.iter().map(|&p| ids[p]).collect();
                    ids.push(node(&store, parents, &i.to_string()).await);
                }
                let mut gens: HashMap<ChangesetId, u64> = HashMap::new();
                for id in &ids {
                    gens.insert(*id, store.generation(REPO, *id).await.unwrap().unwrap());
                    prop_assert_eq!(d.derive::<SkipList>(*id).await.unwrap().generation, gens[id]);
                }
                for (ia, ib) in probes {
                    let a = ids[ia.index(ids.len())];
                    let b = ids[ib.index(ids.len())];
                    let anc_b = ancestors(&store, b).await;
                    prop_assert_eq!(SkipList::is_ancestor(&d, a, b).await.unwrap(), anc_b.contains(&a), "is_ancestor");
                    prop_assert_eq!(
                        SkipList::merge_base(&d, a, b).await.unwrap(),
                        naive_merge_base(&store, a, b).await,
                        "merge_base"
                    );
                    // frontier_at(b, g) is exactly the ancestors of b at generation g.
                    let g = gens[&a];
                    let expected: BTreeSet<ChangesetId> = anc_b.iter().copied().filter(|n| gens[n] == g).collect();
                    prop_assert_eq!(SkipList::frontier_at(&d, b, g).await.unwrap(), expected, "frontier_at");
                }
                Ok(())
            })?;
        }
    }
}
