// Copyright 2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashSet};
use std::fmt;
use std::iter::Peekable;
use std::ops::Range;
use std::sync::Arc;

use itertools::Itertools;

use crate::backend::{ChangeId, CommitId, MillisSinceEpoch, ObjectId};
use crate::default_index_store::{
    CompositeIndex, IndexEntry, IndexEntryByPosition, IndexPosition, RevWalk,
};
use crate::default_revset_graph_iterator::RevsetGraphIterator;
use crate::index::{HexPrefix, Index, PrefixResolution};
use crate::matchers::{EverythingMatcher, Matcher, PrefixMatcher, Visit};
use crate::repo_path::RepoPath;
use crate::revset::{
    ChangeIdIndex, ResolvedExpression, ResolvedPredicateExpression, Revset, RevsetEvaluationError,
    RevsetFilterPredicate, RevsetGraphEdge, GENERATION_RANGE_FULL,
};
use crate::store::Store;
use crate::{backend, rewrite};

trait ToPredicateFn: fmt::Debug {
    /// Creates function that tests if the given entry is included in the set.
    ///
    /// The predicate function is evaluated in order of `RevsetIterator`.
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_>;
}

impl<T: ToPredicateFn + ?Sized> ToPredicateFn for Box<T> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        <T as ToPredicateFn>::to_predicate_fn(self)
    }
}

trait InternalRevset<'index>: fmt::Debug + ToPredicateFn {
    // All revsets currently iterate in order of descending index position
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_>;

    fn into_predicate<'a>(self: Box<Self>) -> Box<dyn ToPredicateFn + 'a>
    where
        Self: 'a;
}

pub struct RevsetImpl<'index> {
    inner: Box<dyn InternalRevset<'index> + 'index>,
    index: CompositeIndex<'index>,
}

impl<'index> RevsetImpl<'index> {
    fn new(
        revset: Box<dyn InternalRevset<'index> + 'index>,
        index: CompositeIndex<'index>,
    ) -> Self {
        Self {
            inner: revset,
            index,
        }
    }

    pub fn iter_graph_impl(&self) -> RevsetGraphIterator<'_, 'index> {
        RevsetGraphIterator::new(self.inner.iter())
    }
}

impl fmt::Debug for RevsetImpl<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevsetImpl")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<'index> Revset<'index> for RevsetImpl<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = CommitId> + '_> {
        Box::new(self.inner.iter().map(|index_entry| index_entry.commit_id()))
    }

    fn iter_graph(&self) -> Box<dyn Iterator<Item = (CommitId, Vec<RevsetGraphEdge>)> + '_> {
        Box::new(RevsetGraphIterator::new(self.inner.iter()))
    }

    fn change_id_index(&self) -> Box<dyn ChangeIdIndex + 'index> {
        // TODO: Create a persistent lookup from change id to commit ids.
        let mut pos_by_change = vec![];
        for entry in self.inner.iter() {
            pos_by_change.push((entry.change_id(), entry.position()));
        }
        let pos_by_change = IdIndex::from_vec(pos_by_change);
        Box::new(ChangeIdIndexImpl {
            index: self.index.clone(),
            pos_by_change,
        })
    }

    fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

struct ChangeIdIndexImpl<'index> {
    index: CompositeIndex<'index>,
    pos_by_change: IdIndex<ChangeId, IndexPosition>,
}

impl ChangeIdIndex for ChangeIdIndexImpl<'_> {
    fn resolve_prefix(&self, prefix: &HexPrefix) -> PrefixResolution<Vec<CommitId>> {
        self.pos_by_change
            .resolve_prefix_with(prefix, |pos| self.index.entry_by_pos(*pos).commit_id())
    }

    fn shortest_unique_prefix_len(&self, change_id: &ChangeId) -> usize {
        self.pos_by_change.shortest_unique_prefix_len(change_id)
    }
}

#[derive(Debug, Clone)]
struct IdIndex<K, V>(Vec<(K, V)>);

impl<K, V> IdIndex<K, V>
where
    K: ObjectId + Ord,
{
    /// Creates new index from the given entries. Multiple values can be
    /// associated with a single key.
    pub fn from_vec(mut vec: Vec<(K, V)>) -> Self {
        vec.sort_unstable_by(|(k0, _), (k1, _)| k0.cmp(k1));
        IdIndex(vec)
    }

    /// Looks up entries with the given prefix, and collects values if matched
    /// entries have unambiguous keys.
    pub fn resolve_prefix_with<U>(
        &self,
        prefix: &HexPrefix,
        mut value_mapper: impl FnMut(&V) -> U,
    ) -> PrefixResolution<Vec<U>> {
        let mut range = self.resolve_prefix_range(prefix).peekable();
        if let Some((first_key, _)) = range.peek().copied() {
            let maybe_entries: Option<Vec<_>> = range
                .map(|(k, v)| (k == first_key).then(|| value_mapper(v)))
                .collect();
            if let Some(entries) = maybe_entries {
                PrefixResolution::SingleMatch(entries)
            } else {
                PrefixResolution::AmbiguousMatch
            }
        } else {
            PrefixResolution::NoMatch
        }
    }

    /// Iterates over entries with the given prefix.
    pub fn resolve_prefix_range<'a: 'b, 'b>(
        &'a self,
        prefix: &'b HexPrefix,
    ) -> impl Iterator<Item = (&'a K, &'a V)> + 'b {
        let min_bytes = prefix.min_prefix_bytes();
        let pos = self.0.partition_point(|(k, _)| k.as_bytes() < min_bytes);
        self.0[pos..]
            .iter()
            .take_while(|(k, _)| prefix.matches(k))
            .map(|(k, v)| (k, v))
    }

    /// This function returns the shortest length of a prefix of `key` that
    /// disambiguates it from every other key in the index.
    ///
    /// The length to be returned is a number of hexadecimal digits.
    ///
    /// This has some properties that we do not currently make much use of:
    ///
    /// - The algorithm works even if `key` itself is not in the index.
    ///
    /// - In the special case when there are keys in the trie for which our
    ///   `key` is an exact prefix, returns `key.len() + 1`. Conceptually, in
    ///   order to disambiguate, you need every letter of the key *and* the
    ///   additional fact that it's the entire key). This case is extremely
    ///   unlikely for hashes with 12+ hexadecimal characters.
    pub fn shortest_unique_prefix_len(&self, key: &K) -> usize {
        let pos = self.0.partition_point(|(k, _)| k < key);
        let left = pos.checked_sub(1).map(|p| &self.0[p]);
        let right = self.0[pos..].iter().find(|(k, _)| k != key);
        itertools::chain(left, right)
            .map(|(neighbor, _value)| {
                backend::common_hex_len(key.as_bytes(), neighbor.as_bytes()) + 1
            })
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug)]
struct EagerRevset<'index> {
    index_entries: Vec<IndexEntry<'index>>,
}

impl EagerRevset<'static> {
    pub const fn empty() -> Self {
        EagerRevset {
            index_entries: Vec::new(),
        }
    }
}

impl<'index> InternalRevset<'index> for EagerRevset<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(self.index_entries.iter().cloned())
    }

    fn into_predicate<'a>(self: Box<Self>) -> Box<dyn ToPredicateFn + 'a>
    where
        Self: 'a,
    {
        self
    }
}

impl ToPredicateFn for EagerRevset<'_> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        predicate_fn_from_iter(self.iter())
    }
}

struct RevWalkRevset<T> {
    walk: T,
}

impl<T> fmt::Debug for RevWalkRevset<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevWalkRevset").finish_non_exhaustive()
    }
}

impl<'index, T> InternalRevset<'index> for RevWalkRevset<T>
where
    T: Iterator<Item = IndexEntry<'index>> + Clone,
{
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(self.walk.clone())
    }

    fn into_predicate<'a>(self: Box<Self>) -> Box<dyn ToPredicateFn + 'a>
    where
        Self: 'a,
    {
        self
    }
}

impl<'index, T> ToPredicateFn for RevWalkRevset<T>
where
    T: Iterator<Item = IndexEntry<'index>> + Clone,
{
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        predicate_fn_from_iter(self.walk.clone())
    }
}

fn predicate_fn_from_iter<'index, 'iter>(
    iter: impl Iterator<Item = IndexEntry<'index>> + 'iter,
) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + 'iter> {
    let mut iter = iter.fuse().peekable();
    Box::new(move |entry| {
        while iter.next_if(|e| e.position() > entry.position()).is_some() {
            continue;
        }
        iter.next_if(|e| e.position() == entry.position()).is_some()
    })
}

#[derive(Debug)]
struct FilterRevset<'index, P> {
    candidates: Box<dyn InternalRevset<'index> + 'index>,
    predicate: P,
}

impl<'index, P: ToPredicateFn> InternalRevset<'index> for FilterRevset<'index, P> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        let p = self.predicate.to_predicate_fn();
        Box::new(self.candidates.iter().filter(p))
    }

    fn into_predicate<'a>(self: Box<Self>) -> Box<dyn ToPredicateFn + 'a>
    where
        Self: 'a,
    {
        self
    }
}

impl<P: ToPredicateFn> ToPredicateFn for FilterRevset<'_, P> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        let mut p1 = self.candidates.to_predicate_fn();
        let mut p2 = self.predicate.to_predicate_fn();
        Box::new(move |entry| p1(entry) && p2(entry))
    }
}

#[derive(Debug)]
struct NotInPredicate<S>(S);

impl<S: ToPredicateFn> ToPredicateFn for NotInPredicate<S> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        let mut p = self.0.to_predicate_fn();
        Box::new(move |entry| !p(entry))
    }
}

#[derive(Debug)]
struct UnionRevset<'index> {
    set1: Box<dyn InternalRevset<'index> + 'index>,
    set2: Box<dyn InternalRevset<'index> + 'index>,
}

impl<'index> InternalRevset<'index> for UnionRevset<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(UnionRevsetIterator {
            iter1: self.set1.iter().peekable(),
            iter2: self.set2.iter().peekable(),
        })
    }

    fn into_predicate<'a>(self: Box<Self>) -> Box<dyn ToPredicateFn + 'a>
    where
        Self: 'a,
    {
        self
    }
}

impl ToPredicateFn for UnionRevset<'_> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        let mut p1 = self.set1.to_predicate_fn();
        let mut p2 = self.set2.to_predicate_fn();
        Box::new(move |entry| p1(entry) || p2(entry))
    }
}

#[derive(Debug)]
struct UnionPredicate<S1, S2> {
    set1: S1,
    set2: S2,
}

impl<S1, S2> ToPredicateFn for UnionPredicate<S1, S2>
where
    S1: ToPredicateFn,
    S2: ToPredicateFn,
{
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        let mut p1 = self.set1.to_predicate_fn();
        let mut p2 = self.set2.to_predicate_fn();
        Box::new(move |entry| p1(entry) || p2(entry))
    }
}

struct UnionRevsetIterator<
    'index,
    I1: Iterator<Item = IndexEntry<'index>>,
    I2: Iterator<Item = IndexEntry<'index>>,
> {
    iter1: Peekable<I1>,
    iter2: Peekable<I2>,
}

impl<'index, I1: Iterator<Item = IndexEntry<'index>>, I2: Iterator<Item = IndexEntry<'index>>>
    Iterator for UnionRevsetIterator<'index, I1, I2>
{
    type Item = IndexEntry<'index>;

    fn next(&mut self) -> Option<Self::Item> {
        match (self.iter1.peek(), self.iter2.peek()) {
            (None, _) => self.iter2.next(),
            (_, None) => self.iter1.next(),
            (Some(entry1), Some(entry2)) => match entry1.position().cmp(&entry2.position()) {
                Ordering::Less => self.iter2.next(),
                Ordering::Equal => {
                    self.iter1.next();
                    self.iter2.next()
                }
                Ordering::Greater => self.iter1.next(),
            },
        }
    }
}

#[derive(Debug)]
struct IntersectionRevset<'index> {
    set1: Box<dyn InternalRevset<'index> + 'index>,
    set2: Box<dyn InternalRevset<'index> + 'index>,
}

impl<'index> InternalRevset<'index> for IntersectionRevset<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(IntersectionRevsetIterator {
            iter1: self.set1.iter().peekable(),
            iter2: self.set2.iter().peekable(),
        })
    }

    fn into_predicate<'a>(self: Box<Self>) -> Box<dyn ToPredicateFn + 'a>
    where
        Self: 'a,
    {
        self
    }
}

impl ToPredicateFn for IntersectionRevset<'_> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        let mut p1 = self.set1.to_predicate_fn();
        let mut p2 = self.set2.to_predicate_fn();
        Box::new(move |entry| p1(entry) && p2(entry))
    }
}

struct IntersectionRevsetIterator<
    'index,
    I1: Iterator<Item = IndexEntry<'index>>,
    I2: Iterator<Item = IndexEntry<'index>>,
> {
    iter1: Peekable<I1>,
    iter2: Peekable<I2>,
}

impl<'index, I1: Iterator<Item = IndexEntry<'index>>, I2: Iterator<Item = IndexEntry<'index>>>
    Iterator for IntersectionRevsetIterator<'index, I1, I2>
{
    type Item = IndexEntry<'index>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match (self.iter1.peek(), self.iter2.peek()) {
                (None, _) => {
                    return None;
                }
                (_, None) => {
                    return None;
                }
                (Some(entry1), Some(entry2)) => match entry1.position().cmp(&entry2.position()) {
                    Ordering::Less => {
                        self.iter2.next();
                    }
                    Ordering::Equal => {
                        self.iter1.next();
                        return self.iter2.next();
                    }
                    Ordering::Greater => {
                        self.iter1.next();
                    }
                },
            }
        }
    }
}

#[derive(Debug)]
struct DifferenceRevset<'index> {
    // The minuend (what to subtract from)
    set1: Box<dyn InternalRevset<'index> + 'index>,
    // The subtrahend (what to subtract)
    set2: Box<dyn InternalRevset<'index> + 'index>,
}

impl<'index> InternalRevset<'index> for DifferenceRevset<'index> {
    fn iter(&self) -> Box<dyn Iterator<Item = IndexEntry<'index>> + '_> {
        Box::new(DifferenceRevsetIterator {
            iter1: self.set1.iter().peekable(),
            iter2: self.set2.iter().peekable(),
        })
    }

    fn into_predicate<'a>(self: Box<Self>) -> Box<dyn ToPredicateFn + 'a>
    where
        Self: 'a,
    {
        self
    }
}

impl ToPredicateFn for DifferenceRevset<'_> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        let mut p1 = self.set1.to_predicate_fn();
        let mut p2 = self.set2.to_predicate_fn();
        Box::new(move |entry| p1(entry) && !p2(entry))
    }
}

struct DifferenceRevsetIterator<
    'index,
    I1: Iterator<Item = IndexEntry<'index>>,
    I2: Iterator<Item = IndexEntry<'index>>,
> {
    iter1: Peekable<I1>,
    iter2: Peekable<I2>,
}

impl<'index, I1: Iterator<Item = IndexEntry<'index>>, I2: Iterator<Item = IndexEntry<'index>>>
    Iterator for DifferenceRevsetIterator<'index, I1, I2>
{
    type Item = IndexEntry<'index>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match (self.iter1.peek(), self.iter2.peek()) {
                (None, _) => {
                    return None;
                }
                (_, None) => {
                    return self.iter1.next();
                }
                (Some(entry1), Some(entry2)) => match entry1.position().cmp(&entry2.position()) {
                    Ordering::Less => {
                        self.iter2.next();
                    }
                    Ordering::Equal => {
                        self.iter2.next();
                        self.iter1.next();
                    }
                    Ordering::Greater => {
                        return self.iter1.next();
                    }
                },
            }
        }
    }
}

// TODO: Having to pass both `&dyn Index` and `CompositeIndex` is a bit ugly.
// Maybe we should make `CompositeIndex` implement `Index`?
pub fn evaluate<'index>(
    expression: &ResolvedExpression,
    store: &Arc<Store>,
    index: &'index dyn Index,
    composite_index: CompositeIndex<'index>,
) -> Result<RevsetImpl<'index>, RevsetEvaluationError> {
    let context = EvaluationContext {
        store: store.clone(),
        index,
        composite_index: composite_index.clone(),
    };
    let internal_revset = context.evaluate(expression)?;
    Ok(RevsetImpl::new(internal_revset, composite_index))
}

struct EvaluationContext<'index> {
    store: Arc<Store>,
    index: &'index dyn Index,
    composite_index: CompositeIndex<'index>,
}

fn to_u32_generation_range(range: &Range<u64>) -> Result<Range<u32>, RevsetEvaluationError> {
    let start = range.start.try_into().map_err(|_| {
        RevsetEvaluationError::Other(format!(
            "Lower bound of generation ({}) is too large",
            range.start
        ))
    })?;
    let end = range.end.try_into().unwrap_or(u32::MAX);
    Ok(start..end)
}

impl<'index> EvaluationContext<'index> {
    fn evaluate(
        &self,
        expression: &ResolvedExpression,
    ) -> Result<Box<dyn InternalRevset<'index> + 'index>, RevsetEvaluationError> {
        match expression {
            ResolvedExpression::Commits(commit_ids) => {
                Ok(Box::new(self.revset_for_commit_ids(commit_ids)))
            }
            ResolvedExpression::Ancestors { heads, generation } => {
                let head_set = self.evaluate(heads)?;
                let walk = self.walk_ancestors(&*head_set);
                if generation == &GENERATION_RANGE_FULL {
                    Ok(Box::new(RevWalkRevset { walk }))
                } else {
                    let walk = walk.filter_by_generation(to_u32_generation_range(generation)?);
                    Ok(Box::new(RevWalkRevset { walk }))
                }
            }
            ResolvedExpression::Range {
                roots,
                heads,
                generation,
            } => {
                let root_set = self.evaluate(roots)?;
                let root_ids = root_set.iter().map(|entry| entry.commit_id()).collect_vec();
                let head_set = self.evaluate(heads)?;
                let head_ids = head_set.iter().map(|entry| entry.commit_id()).collect_vec();
                let walk = self.composite_index.walk_revs(&head_ids, &root_ids);
                if generation == &GENERATION_RANGE_FULL {
                    Ok(Box::new(RevWalkRevset { walk }))
                } else {
                    let walk = walk.filter_by_generation(to_u32_generation_range(generation)?);
                    Ok(Box::new(RevWalkRevset { walk }))
                }
            }
            ResolvedExpression::DagRange {
                roots,
                heads,
                generation_from_roots,
            } => {
                let root_set = self.evaluate(roots)?;
                let head_set = self.evaluate(heads)?;
                if generation_from_roots == &(1..2) {
                    Ok(Box::new(self.walk_children(&*root_set, &*head_set)))
                } else if generation_from_roots == &GENERATION_RANGE_FULL {
                    let (dag_range_set, _) = self.collect_dag_range(&*root_set, &*head_set);
                    Ok(Box::new(dag_range_set))
                } else {
                    // For small generation range, it might be better to build a reachable map
                    // with generation bit set, which can be calculated incrementally from roots:
                    //   reachable[pos] = (reachable[parent_pos] | ...) << 1
                    let root_positions =
                        root_set.iter().map(|entry| entry.position()).collect_vec();
                    let walk = self
                        .walk_ancestors(&*head_set)
                        .descendants_filtered_by_generation(
                            &root_positions,
                            to_u32_generation_range(generation_from_roots)?,
                        );
                    let mut index_entries = walk.collect_vec();
                    index_entries.reverse();
                    Ok(Box::new(EagerRevset { index_entries }))
                }
            }
            ResolvedExpression::Heads(candidates) => {
                let candidate_set = self.evaluate(candidates)?;
                let candidate_ids = candidate_set
                    .iter()
                    .map(|entry| entry.commit_id())
                    .collect_vec();
                Ok(Box::new(self.revset_for_commit_ids(
                    &self.composite_index.heads(&mut candidate_ids.iter()),
                )))
            }
            ResolvedExpression::Roots(candidates) => {
                let candidate_set = EagerRevset {
                    index_entries: self.evaluate(candidates)?.iter().collect(),
                };
                let (_, filled) = self.collect_dag_range(&candidate_set, &candidate_set);
                let mut index_entries = vec![];
                for candidate in candidate_set.iter() {
                    if !candidate
                        .parent_positions()
                        .iter()
                        .any(|parent| filled.contains(parent))
                    {
                        index_entries.push(candidate);
                    }
                }
                Ok(Box::new(EagerRevset { index_entries }))
            }
            ResolvedExpression::Latest { candidates, count } => {
                let candidate_set = self.evaluate(candidates)?;
                Ok(Box::new(
                    self.take_latest_revset(candidate_set.as_ref(), *count),
                ))
            }
            ResolvedExpression::Union(expression1, expression2) => {
                let set1 = self.evaluate(expression1)?;
                let set2 = self.evaluate(expression2)?;
                Ok(Box::new(UnionRevset { set1, set2 }))
            }
            ResolvedExpression::FilterWithin {
                candidates,
                predicate,
            } => Ok(Box::new(FilterRevset {
                candidates: self.evaluate(candidates)?,
                predicate: self.evaluate_predicate(predicate)?,
            })),
            ResolvedExpression::Intersection(expression1, expression2) => {
                let set1 = self.evaluate(expression1)?;
                let set2 = self.evaluate(expression2)?;
                Ok(Box::new(IntersectionRevset { set1, set2 }))
            }
            ResolvedExpression::Difference(expression1, expression2) => {
                let set1 = self.evaluate(expression1)?;
                let set2 = self.evaluate(expression2)?;
                Ok(Box::new(DifferenceRevset { set1, set2 }))
            }
        }
    }

    fn evaluate_predicate(
        &self,
        expression: &ResolvedPredicateExpression,
    ) -> Result<Box<dyn ToPredicateFn + 'index>, RevsetEvaluationError> {
        match expression {
            ResolvedPredicateExpression::Filter(predicate) => Ok(build_predicate_fn(
                self.store.clone(),
                self.index,
                predicate,
            )),
            ResolvedPredicateExpression::Set(expression) => {
                Ok(self.evaluate(expression)?.into_predicate())
            }
            ResolvedPredicateExpression::NotIn(complement) => {
                let set = self.evaluate_predicate(complement)?;
                Ok(Box::new(NotInPredicate(set)))
            }
            ResolvedPredicateExpression::Union(expression1, expression2) => {
                let set1 = self.evaluate_predicate(expression1)?;
                let set2 = self.evaluate_predicate(expression2)?;
                Ok(Box::new(UnionPredicate { set1, set2 }))
            }
        }
    }

    fn walk_ancestors<'a, S>(&self, head_set: &S) -> RevWalk<'index>
    where
        S: InternalRevset<'a> + ?Sized,
    {
        let head_ids = head_set.iter().map(|entry| entry.commit_id()).collect_vec();
        self.composite_index.walk_revs(&head_ids, &[])
    }

    fn walk_children<'a, 'b, S, T>(
        &self,
        root_set: &S,
        head_set: &T,
    ) -> impl InternalRevset<'index> + 'index
    where
        S: InternalRevset<'a> + ?Sized,
        T: InternalRevset<'b> + ?Sized,
    {
        let root_positions = root_set.iter().map(|entry| entry.position()).collect_vec();
        let walk = self
            .walk_ancestors(head_set)
            .take_until_roots(&root_positions);
        let root_positions: HashSet<_> = root_positions.into_iter().collect();
        let candidates = Box::new(RevWalkRevset { walk });
        let predicate = PurePredicateFn(move |entry: &IndexEntry| {
            entry
                .parent_positions()
                .iter()
                .any(|parent_pos| root_positions.contains(parent_pos))
        });
        // TODO: Suppose heads include all visible heads, ToPredicateFn version can be
        // optimized to only test the predicate()
        FilterRevset {
            candidates,
            predicate,
        }
    }

    /// Calculates `root_set:head_set`.
    fn collect_dag_range<'a, 'b, S, T>(
        &self,
        root_set: &S,
        head_set: &T,
    ) -> (EagerRevset<'index>, HashSet<IndexPosition>)
    where
        S: InternalRevset<'a> + ?Sized,
        T: InternalRevset<'b> + ?Sized,
    {
        let root_positions = root_set.iter().map(|entry| entry.position()).collect_vec();
        let walk = self
            .walk_ancestors(head_set)
            .take_until_roots(&root_positions);
        let root_positions: HashSet<_> = root_positions.into_iter().collect();
        let mut reachable_positions = HashSet::new();
        let mut index_entries = vec![];
        for candidate in walk.collect_vec().into_iter().rev() {
            if root_positions.contains(&candidate.position())
                || candidate
                    .parent_positions()
                    .iter()
                    .any(|parent_pos| reachable_positions.contains(parent_pos))
            {
                reachable_positions.insert(candidate.position());
                index_entries.push(candidate);
            }
        }
        index_entries.reverse();
        (EagerRevset { index_entries }, reachable_positions)
    }

    fn revset_for_commit_ids(&self, commit_ids: &[CommitId]) -> EagerRevset<'index> {
        let mut index_entries = vec![];
        for id in commit_ids {
            index_entries.push(self.composite_index.entry_by_id(id).unwrap());
        }
        index_entries.sort_unstable_by_key(|b| Reverse(b.position()));
        index_entries.dedup();
        EagerRevset { index_entries }
    }

    fn take_latest_revset(
        &self,
        candidate_set: &dyn InternalRevset<'index>,
        count: usize,
    ) -> EagerRevset<'index> {
        if count == 0 {
            return EagerRevset::empty();
        }

        #[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
        struct Item<'a> {
            timestamp: MillisSinceEpoch,
            entry: IndexEntryByPosition<'a>, // tie-breaker
        }

        let make_rev_item = |entry: IndexEntry<'index>| {
            let commit = self.store.get_commit(&entry.commit_id()).unwrap();
            Reverse(Item {
                timestamp: commit.committer().timestamp.timestamp.clone(),
                entry: IndexEntryByPosition(entry),
            })
        };

        // Maintain min-heap containing the latest (greatest) count items. For small
        // count and large candidate set, this is probably cheaper than building vec
        // and applying selection algorithm.
        let mut candidate_iter = candidate_set.iter().map(make_rev_item).fuse();
        let mut latest_items = BinaryHeap::from_iter(candidate_iter.by_ref().take(count));
        for item in candidate_iter {
            let mut earliest = latest_items.peek_mut().unwrap();
            if earliest.0 < item.0 {
                *earliest = item;
            }
        }

        assert!(latest_items.len() <= count);
        let mut index_entries = latest_items
            .into_iter()
            .map(|item| item.0.entry.0)
            .collect_vec();
        index_entries.sort_unstable_by_key(|b| Reverse(b.position()));
        EagerRevset { index_entries }
    }
}

struct PurePredicateFn<F>(F);

impl<F> fmt::Debug for PurePredicateFn<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PurePredicateFn").finish_non_exhaustive()
    }
}

impl<F: Fn(&IndexEntry<'_>) -> bool> ToPredicateFn for PurePredicateFn<F> {
    fn to_predicate_fn(&self) -> Box<dyn FnMut(&IndexEntry<'_>) -> bool + '_> {
        Box::new(&self.0)
    }
}

fn pure_predicate_fn<'index>(
    f: impl Fn(&IndexEntry<'_>) -> bool + 'index,
) -> Box<dyn ToPredicateFn + 'index> {
    Box::new(PurePredicateFn(f))
}

fn build_predicate_fn<'index>(
    store: Arc<Store>,
    index: &'index dyn Index,
    predicate: &RevsetFilterPredicate,
) -> Box<dyn ToPredicateFn + 'index> {
    match predicate {
        RevsetFilterPredicate::ParentCount(parent_count_range) => {
            let parent_count_range = parent_count_range.clone();
            pure_predicate_fn(move |entry| parent_count_range.contains(&entry.num_parents()))
        }
        RevsetFilterPredicate::Description(needle) => {
            let needle = needle.clone();
            pure_predicate_fn(move |entry| {
                store
                    .get_commit(&entry.commit_id())
                    .unwrap()
                    .description()
                    .contains(needle.as_str())
            })
        }
        RevsetFilterPredicate::Author(needle) => {
            let needle = needle.clone();
            // TODO: Make these functions that take a needle to search for accept some
            // syntax for specifying whether it's a regex and whether it's
            // case-sensitive.
            pure_predicate_fn(move |entry| {
                let commit = store.get_commit(&entry.commit_id()).unwrap();
                commit.author().name.contains(needle.as_str())
                    || commit.author().email.contains(needle.as_str())
            })
        }
        RevsetFilterPredicate::Committer(needle) => {
            let needle = needle.clone();
            pure_predicate_fn(move |entry| {
                let commit = store.get_commit(&entry.commit_id()).unwrap();
                commit.committer().name.contains(needle.as_str())
                    || commit.committer().email.contains(needle.as_str())
            })
        }
        RevsetFilterPredicate::File(paths) => {
            // TODO: Add support for globs and other formats
            let matcher: Box<dyn Matcher> = if let Some(paths) = paths {
                Box::new(PrefixMatcher::new(paths))
            } else {
                Box::new(EverythingMatcher)
            };
            pure_predicate_fn(move |entry| {
                has_diff_from_parent(&store, index, entry, matcher.as_ref())
            })
        }
        RevsetFilterPredicate::HasConflict => pure_predicate_fn(move |entry| {
            let commit = store.get_commit(&entry.commit_id()).unwrap();
            commit.tree().has_conflict()
        }),
    }
}

fn has_diff_from_parent(
    store: &Arc<Store>,
    index: &dyn Index,
    entry: &IndexEntry<'_>,
    matcher: &dyn Matcher,
) -> bool {
    let commit = store.get_commit(&entry.commit_id()).unwrap();
    let parents = commit.parents();
    if let [parent] = parents.as_slice() {
        // Fast path: no need to load the root tree
        let unchanged = commit.tree_id() == parent.tree_id();
        if matcher.visit(&RepoPath::root()) == Visit::AllRecursively {
            return !unchanged;
        } else if unchanged {
            return false;
        }
    }
    let from_tree = rewrite::merge_commit_trees_without_repo(store, index, &parents);
    let to_tree = commit.tree();
    from_tree.diff(&to_tree, matcher).next().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ChangeId, CommitId, ObjectId};
    use crate::default_index_store::MutableIndexImpl;

    #[test]
    fn test_id_index_resolve_prefix() {
        fn sorted(resolution: PrefixResolution<Vec<i32>>) -> PrefixResolution<Vec<i32>> {
            match resolution {
                PrefixResolution::SingleMatch(mut xs) => {
                    xs.sort(); // order of values might not be preserved by IdIndex
                    PrefixResolution::SingleMatch(xs)
                }
                _ => resolution,
            }
        }
        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("0000"), 0),
            (ChangeId::from_hex("0099"), 1),
            (ChangeId::from_hex("0099"), 2),
            (ChangeId::from_hex("0aaa"), 3),
            (ChangeId::from_hex("0aab"), 4),
        ]);
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("00").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("000").unwrap(), |&v| v),
            PrefixResolution::SingleMatch(vec![0]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0001").unwrap(), |&v| v),
            PrefixResolution::NoMatch,
        );
        assert_eq!(
            sorted(id_index.resolve_prefix_with(&HexPrefix::new("009").unwrap(), |&v| v)),
            PrefixResolution::SingleMatch(vec![1, 2]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0aa").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0aab").unwrap(), |&v| v),
            PrefixResolution::SingleMatch(vec![4]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("f").unwrap(), |&v| v),
            PrefixResolution::NoMatch,
        );
    }

    #[test]
    fn test_id_index_shortest_unique_prefix_len() {
        // No crash if empty
        let id_index = IdIndex::from_vec(vec![] as Vec<(ChangeId, ())>);
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("00")),
            0
        );

        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("ab"), ()),
            (ChangeId::from_hex("acd0"), ()),
            (ChangeId::from_hex("acd0"), ()), // duplicated key is allowed
        ]);
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("acd0")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ac")),
            3
        );

        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("ab"), ()),
            (ChangeId::from_hex("acd0"), ()),
            (ChangeId::from_hex("acf0"), ()),
            (ChangeId::from_hex("a0"), ()),
            (ChangeId::from_hex("ba"), ()),
        ]);

        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("a0")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ba")),
            1
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ab")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("acd0")),
            3
        );
        // If it were there, the length would be 1.
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("c0")),
            1
        );
    }

    /// Generator of unique 16-byte ChangeId excluding root id
    fn change_id_generator() -> impl FnMut() -> ChangeId {
        let mut iter = (1_u128..).map(|n| ChangeId::new(n.to_le_bytes().into()));
        move || iter.next().unwrap()
    }

    #[test]
    fn test_revset_combinator() {
        let mut new_change_id = change_id_generator();
        let mut index = MutableIndexImpl::full(3, 16);
        let id_0 = CommitId::from_hex("000000");
        let id_1 = CommitId::from_hex("111111");
        let id_2 = CommitId::from_hex("222222");
        let id_3 = CommitId::from_hex("333333");
        let id_4 = CommitId::from_hex("444444");
        index.add_commit_data(id_0.clone(), new_change_id(), &[]);
        index.add_commit_data(id_1.clone(), new_change_id(), &[id_0.clone()]);
        index.add_commit_data(id_2.clone(), new_change_id(), &[id_1.clone()]);
        index.add_commit_data(id_3.clone(), new_change_id(), &[id_2.clone()]);
        index.add_commit_data(id_4.clone(), new_change_id(), &[id_3.clone()]);

        let get_entry = |id: &CommitId| index.as_composite().entry_by_id(id).unwrap();
        let make_entries = |ids: &[&CommitId]| ids.iter().map(|id| get_entry(id)).collect_vec();
        let make_set = |ids: &[&CommitId]| -> Box<dyn InternalRevset> {
            let index_entries = make_entries(ids);
            Box::new(EagerRevset { index_entries })
        };

        let set = make_set(&[&id_4, &id_3, &id_2, &id_0]);
        let mut p = set.to_predicate_fn();
        assert!(p(&get_entry(&id_4)));
        assert!(p(&get_entry(&id_3)));
        assert!(p(&get_entry(&id_2)));
        assert!(!p(&get_entry(&id_1)));
        assert!(p(&get_entry(&id_0)));
        // Uninteresting entries can be skipped
        let mut p = set.to_predicate_fn();
        assert!(p(&get_entry(&id_3)));
        assert!(!p(&get_entry(&id_1)));
        assert!(p(&get_entry(&id_0)));

        let set = FilterRevset {
            candidates: make_set(&[&id_4, &id_2, &id_0]),
            predicate: pure_predicate_fn(|entry| entry.commit_id() != id_4),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_2, &id_0]));
        let mut p = set.to_predicate_fn();
        assert!(!p(&get_entry(&id_4)));
        assert!(!p(&get_entry(&id_3)));
        assert!(p(&get_entry(&id_2)));
        assert!(!p(&get_entry(&id_1)));
        assert!(p(&get_entry(&id_0)));

        // Intersection by FilterRevset
        let set = FilterRevset {
            candidates: make_set(&[&id_4, &id_2, &id_0]),
            predicate: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_2]));
        let mut p = set.to_predicate_fn();
        assert!(!p(&get_entry(&id_4)));
        assert!(!p(&get_entry(&id_3)));
        assert!(p(&get_entry(&id_2)));
        assert!(!p(&get_entry(&id_1)));
        assert!(!p(&get_entry(&id_0)));

        let set = UnionRevset {
            set1: make_set(&[&id_4, &id_2]),
            set2: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(
            set.iter().collect_vec(),
            make_entries(&[&id_4, &id_3, &id_2, &id_1])
        );
        let mut p = set.to_predicate_fn();
        assert!(p(&get_entry(&id_4)));
        assert!(p(&get_entry(&id_3)));
        assert!(p(&get_entry(&id_2)));
        assert!(p(&get_entry(&id_1)));
        assert!(!p(&get_entry(&id_0)));

        let set = IntersectionRevset {
            set1: make_set(&[&id_4, &id_2, &id_0]),
            set2: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_2]));
        let mut p = set.to_predicate_fn();
        assert!(!p(&get_entry(&id_4)));
        assert!(!p(&get_entry(&id_3)));
        assert!(p(&get_entry(&id_2)));
        assert!(!p(&get_entry(&id_1)));
        assert!(!p(&get_entry(&id_0)));

        let set = DifferenceRevset {
            set1: make_set(&[&id_4, &id_2, &id_0]),
            set2: make_set(&[&id_3, &id_2, &id_1]),
        };
        assert_eq!(set.iter().collect_vec(), make_entries(&[&id_4, &id_0]));
        let mut p = set.to_predicate_fn();
        assert!(p(&get_entry(&id_4)));
        assert!(!p(&get_entry(&id_3)));
        assert!(!p(&get_entry(&id_2)));
        assert!(!p(&get_entry(&id_1)));
        assert!(p(&get_entry(&id_0)));
    }
}
