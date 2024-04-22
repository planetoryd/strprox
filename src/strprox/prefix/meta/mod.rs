use std::{
    borrow::{Borrow, Cow},
    cmp::{max, min},
    collections::{btree_map::Entry, hash_map, BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet},
    marker::PhantomData,
    ops::Range,
    sync::{Mutex, RwLock},
    time::Instant,
};

use super::{FromStrings, MeasuredPrefix};
use crate::{levenshtein, Autocompleter};

use debug_print::debug_println;
use polonius_the_crab::{polonius, polonius_return};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use slab::Slab;
use yoke::{Yoke, Yokeable};

//mod compact_tree;

/// Implements "Matching-Based Method for Error-Tolerant Autocompletion" (META) from https://doi.org/10.14778/2977797.2977808

// Arithmetic using generics/traits is cumbersome in Rust
// These are here to have inlay type hints in my IDE, which are missing when a macro is added for them
// They are three repeated letters to easily search and replace later to add macros
/// Type that bounds the length of a stored string
type UUU = u8;
/// Type that bounds the number of stored strings
type SSS = u32;

/// A trie node with a similar structure from META
#[derive(PartialEq, Eq, Hash, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Node<UUU, SSS> {
    /// One Unicode character
    character: char,
    /// Range of indices into descendant nodes
    descendant_range: Range<SSS>,
    /// Range of indices into strings with the prefix from this node
    string_range: Range<SSS>,
    /// Length of the prefix from this node
    depth: UUU,
}

impl Node<UUU, SSS> {
    /// Returns the index into the trie where the node is
    #[inline]
    fn id(&self) -> usize {
        self.descendant_range.start as usize - 1
    }
    #[inline]
    /// Returns the id of the first child/descendant, which is equivalent to the id for sorting
    fn first_descendant_id(&self) -> usize {
        self.descendant_range.start as usize
    }
}

pub type TreeString<'stored> = Cow<'stored, str>;
type TrieStrings<'stored> = Vec<TreeString<'stored>>;
type TrieNodes<UUU, SSS> = Vec<Node<UUU, SSS>>;

pub trait TreeStringT<'a>: 'a + Clone {
    fn from_string(sx: &'a String) -> Self;
    fn to_str<'s>(&'s self) -> &'s str;
    fn from_owned(sx: String) -> Self;
}

impl<'a> TreeStringT<'a> for Cow<'a, str> {
    fn from_string(sx: &'a String) -> Self {
        Cow::Borrowed(sx.as_str())
    }
    fn to_str<'s>(&'s self) -> &'s str {
        &self
    }
    fn from_owned(sx: String) -> Self {
        Cow::Owned(sx)
    }
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Trie<'stored, UUU, SSS> {
    nodes: TrieNodes<UUU, SSS>,
    #[cfg_attr(feature = "serde", serde(borrow))]
    /// Stored strings
    pub strings: TrieStrings<'stored>,
}

/// Returns an Option with the next valid Unicode scalar value after `character`, unless `character` is char::MAX
#[inline]
fn char_succ(character: char) -> Option<char> {
    let mut char_range = character..=char::MAX;
    char_range.nth(1)
}

impl<'stored> Trie<'stored, UUU, SSS> {
    /// Returns the root node of the trie (panics if the trie is empty)
    fn root(&self) -> &Node<UUU, SSS> {
        // this shouldn't be able to panic from the public API
        self.nodes.first().unwrap()
    }
    fn fill_results(&self, node: &Node<UUU, SSS>, result: &mut HashSet<TreeString<'stored>>) {
        for string_index in node.string_range.clone() {
            result.insert(self.strings[string_index as usize].clone());
        }
    }
    /// Returns trie over `source` (expects `source` to have at most usize::MAX - 1 strings)
    pub fn new(len: usize, source: impl IntoIterator<Item = TreeString<'stored>>) -> Self {
        let mut strings = TrieStrings::<'stored>::with_capacity(len);
        for string in source.into_iter() {
            strings.push(string);
        }
        // sort and dedup to compute the `string_range` for each node using binary search
        strings.sort();
        strings.dedup();

        // rough estimate on the size of the trie
        let nodes = TrieNodes::with_capacity(3 * len);

        let mut trie = Self { strings, nodes };

        // Construct all nodes
        trie.init_nodes(
            &mut 0,
            0,
            &mut Default::default(),
            '\0',
            0,
            0,
            trie.strings.len(),
        );
        trie
    }
    /// `last_char` is the last character in the prefix
    fn init_nodes(
        &mut self,
        node_id: &mut usize,
        depth: UUU,
        prefix: &mut String,
        last_char: char,
        suffix_start: usize,
        start: usize,
        end: usize,
    ) {
        let current_id = node_id.clone();

        let current_node: Node<u8, u32> = Node::<UUU, SSS> {
            character: last_char,
            // change the descendant range later
            descendant_range: Default::default(),
            string_range: start as SSS..end as SSS,
            depth,
        };
        // the current node is added before all the descendants,
        // and its location in `nodes` is `current_id`
        debug_assert_eq!(self.nodes.len(), current_id);
        self.nodes.push(current_node);

        // the next node, if it exists, will have 1 higher id
        *node_id += 1;

        // `node_id` is required to be incremented in pre-order to have continuous `descendant_range``
        let mut child_start = start;
        while child_start != end {
            // add to the prefix
            let suffix = &self.strings[child_start][suffix_start..];
            if let Some(next_char) = suffix.chars().next() {
                // strings in strings[child_start..child_end] will have the same prefix
                let child_end;
                let next_prefix;

                // get the boundary in `strings` for strings with the prefix extended with next_char
                if let Some(succ) = char_succ(next_char) {
                    // `lexicographic_marker` is the first string that's lexicographically ordered after all strings with prefix
                    let lexicographic_marker = &mut *prefix;
                    lexicographic_marker.push(succ);

                    // offset from start where the lexicographic marker would be
                    let offset;
                    match self.strings[start..end]
                        .binary_search(&TreeStringT::from_string(&lexicographic_marker))
                    {
                        // same bound either way, but if it's Err it will be the last iteration
                        Ok(x) => offset = x,
                        Err(x) => offset = x,
                    }
                    debug_assert_eq!(
                        offset,
                        self.strings[start..end].partition_point(
                            |string| string < &TreeStringT::from_string(&lexicographic_marker)
                        )
                    );
                    child_end = start + offset;

                    debug_assert!(child_end > child_start);

                    next_prefix = lexicographic_marker;
                    next_prefix.pop();
                } else {
                    // the next character in the prefix is char::MAX,
                    // so this must be the last prefix from the current one
                    debug_assert_eq!(next_char, char::MAX);
                    child_end = end;
                    next_prefix = prefix;
                }
                next_prefix.push(next_char);

                // requires nightly
                //let next_suffix_start = strings[start].ceil_char_boundary(suffix_start + 1);

                let next_suffix_start = suffix_start + next_char.len_utf8();

                // Construct all descendant nodes with the next prefix
                self.init_nodes(
                    node_id,
                    depth + 1,
                    next_prefix,
                    next_char,
                    next_suffix_start,
                    child_start,
                    child_end,
                );

                // reset the prefix state
                let prefix = next_prefix;
                prefix.pop();

                // look at strings with a different next character in their prefix
                child_start = child_end;
            } else {
                // this string has already been accounted for by the parent node,
                // whose prefix is equal to the whole string
                child_start += 1;
            }
        }

        // node_id is now 1 greater than the index of the last in-order node that's in the subtree from the current node
        let descendant_range = current_id as SSS + 1..*node_id as SSS;
        self.nodes[current_id].descendant_range = descendant_range;
    }
}

/// Inverted index from META
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
struct InvertedIndex<UUU, SSS> {
    /// depth |-> (character |-> nodes ids in trie)
    index: Vec<HashMap<char, Vec<SSS>>>,
    /// Marker to allow macros to specialize methods for UUU
    u_marker: PhantomData<UUU>,
}

impl InvertedIndex<UUU, SSS> {
    /// Constructs an inverted index from depth to character to nodes using a trie
    fn new(trie: &Trie<UUU, SSS>) -> Self {
        let mut max_depth = 0;
        for node in &trie.nodes {
            max_depth = max(max_depth, node.depth as usize);
        }

        let mut index = Vec::<HashMap<char, Vec<SSS>>>::with_capacity(max_depth + 1);
        index.resize(max_depth + 1, Default::default());

        // put all nodes into the index at a certain depth and character
        for node in &trie.nodes {
            let depth = node.depth as usize;
            let char_map = &mut index[depth];
            if let Some(nodes) = char_map.get_mut(&node.character) {
                nodes.push(node.id() as SSS);
            } else {
                char_map.insert(node.character, vec![node.id() as SSS]);
            }
        }
        // sort the nodes by id for binary search (cache locality with Vec)
        for char_map in &mut index {
            for (_, nodes) in char_map {
                nodes.sort_unstable();
            }
        }
        Self {
            index,
            u_marker: PhantomData,
        }
    }
    /// Returns the node ids with `depth` and `character`
    fn get(&self, depth: usize, character: char) -> Option<&Vec<SSS>> {
        self.index[depth].get(&character)
    }
    /// Returns maximum depth of nodes stored in the index
    fn max_depth(&self) -> usize {
        self.index.len() - 1
    }
}

use ptrie::Trie as PTrie;

/// Structure that allows for autocompletion based on a string dataset
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Yokeable)]
pub struct MetaAutocompleter<'stored, UUU = u8, SSS = u32> {
    #[cfg_attr(feature = "serde", serde(borrow))]
    pub trie: Trie<'stored, UUU, SSS>,
    inverted_index: InvertedIndex<UUU, SSS>,
}

#[derive(Default)]
/// Separate this it out entirely to avoid lifetime conflicts
pub struct Cache<'stored> {
    cached_prefix: PTrie<char, PState>,
    lru: CacheMap<'stored>,
}

impl<'x> Cache<'x> {
    pub fn visit<'t, 'q>(&'t mut self, query: TreeString<'q>) -> Vec<(usize, &'t PState)> {
        let mut ptree = &mut self.cached_prefix;
        let query: TreeString<'q> = polonius!(|ptree| -> Vec<(usize, &'polonius PState)> {
            let li = ptree.find_prefixes(query.chars());
            // Invalidate 'em all
            for (i, PState { prio, sets: _, ix }) in li.iter() {
                let lock = prio.lock().unwrap();
                self.lru.rm(&lock, ix);
            }
            if li.len() == query.len() {
                let (i, tip) = li.last().unwrap();
                let mut lock = tip.prio.lock().unwrap();
                self.lru.rm(&lock, &tip.ix);
                *lock = Instant::now();
                polonius_return!(li)
            }
            query
        });
        ptree.insert(
            query.chars(),
            PState {
                sets: Default::default(),
                prio: Instant::now().into(),
                ix: self
                    .lru
                    .slab
                    .insert(TreeStringT::from_owned(query.to_string())),
            },
        );
        let li = ptree.find_prefixes(query.chars());
        li
    }
}

pub struct PState {
    /// vec index as key, b -> P(i,b) delta
    sets: Vec<MatchingSet<UUU>>,
    /// last visit
    prio: Mutex<Instant>,
    ix: usize,
}

/// Reverse index
#[derive(Default)]
pub struct CacheMap<'s> {
    slab: Slab<TreeString<'s>>,
    /// Priority --> Set: prefix
    /// Ascending, old to new
    prio: BTreeMap<Instant, BTreeSet<usize>>,
}

impl<'s> CacheMap<'s> {
    pub fn rm(&mut self, t: &Instant, k: &usize) -> bool {
        if let Some(set) = self.prio.get_mut(t) {
            set.remove(k);
            true
        } else {
            false
        }
    }
    pub fn add(&mut self, t: Instant, k: usize) {
        match self.prio.entry(t) {
            Entry::Occupied(mut oc) => {
                oc.get_mut().insert(k);
            }
            Entry::Vacant(va) => {
                va.insert([k].into());
            }
        }
    }
}

impl<'stored> MetaAutocompleter<'stored, UUU, SSS> {
    /// Constructs an Autocompleter given the string dataset `source` (does not copy strings)
    pub fn new(len: usize, source: impl IntoIterator<Item = TreeString<'stored>>) -> Self {
        let trie = Trie::<'stored, UUU, SSS>::new(len, source);
        let inverted_index = InvertedIndex::<UUU, SSS>::new(&trie);
        Self {
            trie,
            inverted_index,
        }
    }
    pub fn len(&self) -> usize {
        self.trie.strings.len()
    }

    pub fn prune(&mut self, cache: &'stored mut Cache<'stored>) {
        let max = 1000;
        // oldest element ---- cutoff ----- newest element
        let cutoff = *if cache.lru.prio.len() < max {
            return;
        } else {
            cache.lru.prio.keys().nth_back(max).unwrap()
        };
        for (_k, set) in cache.lru.prio.range(..cutoff).rev() {
            // prune all the tail after each node, cuz every marker node after it must be older/smaller
            for ix in set {
                let prefix = &cache.lru.slab[*ix];
                cache.cached_prefix.remove_subtree(prefix.chars())
            }
        }
        cache.lru.prio = cache.lru.prio.split_off(&cutoff);
    }
    /// P(|q|,b)
    pub fn assemble<'q>(
        &self,
        q: TreeString<'q>,
        b: usize,
        cache: &mut Cache<'_>,
    ) -> MatchingSet<UUU> {
        let query_chars: Vec<char> = q.chars().collect();
        // ----|
        //     |
        let li = cache.visit(q.clone());
        let missing: BTreeSet<_> = li
            .iter()
            .filter(|(_, x)| x.sets.len() < 1)
            .map(|(k, _)| *k)
            .collect();
        drop(li);
        let mut acc = MatchingSet::new_trie(&self.trie);
        for i in 0..q.len() {
            if missing.contains(&i) {
                let s = self.first_deducing(
                    &mut acc,
                    *query_chars.last().unwrap(),
                    query_chars.len(),
                    b,
                );
                let k = cache
                    .cached_prefix
                    .get_mut(query_chars[..=i].iter().map(|k| *k))
                    .unwrap();
                *k = PState {
                    sets: [s].into(),
                    prio: Instant::now().into(),
                    ix: 0,
                };
            }
        }
        drop(acc);

        let li = cache.visit(q.clone());
        let row1: HashMap<(UUU, NodeID), UUU> = li.iter().fold(HashMap::new(), |mut a, (i, n)| {
            a.extend(n.sets[0].matchings.iter());
            a
        });
        let mut row1 = MatchingSet::new(row1);
        // P(|q|,1)

        for t in 2..=b {
            let delta = self.second_deducing(&row1, &query_chars, query_chars.len(), t);
            row1.matchings.extend(delta.matchings)
        }

        row1
    }
}

#[test]
fn try_range() {
    println!("{:?}", (2..=2).into_iter().collect::<Vec<_>>());
}

#[derive(Clone, Copy)]
struct Matching<UUU>
where
    UUU: Clone,
{
    query_prefix_len: UUU,
    node: NodeID,
    edit_distance: UUU,
}

impl<'stored> Matching<UUU> {
    /// Returns an upper bound on the edit distance between the query and a prefix of length `stored_len` that intersects
    /// with the matching node's prefix
    fn deduced_edit_distance(
        &self,
        query_len: usize,
        stored_len: usize,
        nodes: &TrieNodes<UUU, SSS>,
    ) -> usize {
        self.edit_distance as usize
            + max(
                query_len.saturating_sub(self.query_prefix_len as usize),
                stored_len.saturating_sub(nodes[self.node].depth as usize),
            )
    }
    /// Returns an upper bound on the edit distance between the query and the matching node's prefix
    fn deduced_prefix_edit_distance(&self, query_len: usize) -> usize {
        self.edit_distance as usize + query_len - self.query_prefix_len as usize
    }
}

use derive_new::new;

type NodeID = usize;

#[derive(Debug, Default, Clone, new)]
pub struct MatchingSet<UUU>
where
    UUU: Clone,
{
    /// Maps the first two parts of a matching to the edit distance
    pub matchings: HashMap<(UUU, NodeID), UUU>,
}

impl MatchingSet<UUU> {
    /// Inserts `matching` into the MatchingSet
    fn insert(&mut self, matching: Matching<UUU>) {
        self.matchings.insert(
            (matching.query_prefix_len, matching.node),
            matching.edit_distance,
        );
    }
    /// Returns an iterator over the matchings
    fn iter<'u>(&'u self) -> MatchingSetIter<'u, UUU> {
        MatchingSetIter {
            iter: self.matchings.iter(),
        }
    }
    /// Returns whether there is a matching for `query_prefix_len` and `node`
    fn contains(&self, query_prefix_len: UUU, node: NodeID) -> bool {
        self.matchings.contains_key(&(query_prefix_len, node))
    }
    /// Returns a matching set with a matching for the root of the `trie` and an empty query
    fn new_trie(trie: &Trie<'_, UUU, SSS>) -> Self {
        let mut matchings = HashMap::<(UUU, NodeID), UUU>::new();
        let query_prefix_len = 0;
        let node = trie.root();
        let edit_distance = 0;
        matchings.insert((query_prefix_len, trie.root().id()), edit_distance);
        Self { matchings }
    }
}

/// Iterator over the matchings in a MatchingSet
struct MatchingSetIter<'iter, UUU>
where
    UUU: Clone,
{
    iter: hash_map::Iter<'iter, (UUU, usize), UUU>,
}

impl<'user> Iterator for MatchingSetIter<'user, UUU> {
    type Item = Matching<UUU>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((&(query_prefix_len, node), &edit_distance)) = self.iter.next() {
            Some(Matching {
                query_prefix_len,
                node,
                edit_distance,
            })
        } else {
            None
        }
    }
}

impl<'stored> MetaAutocompleter<'stored, UUU, SSS> {
    /// Returns the top `requested` number of strings with the best prefix distance from the query
    /// sorted by prefix edit distance and then lexicographical order,
    /// or all strings available if `requested` is larger than the number stored
    ///
    /// Assumes `query`'s length in Unicode characters is bounded by UUU; will truncate to UUU::MAX characters otherwise
    pub fn autocomplete(
        &'_ self,
        query: &str,
        cache: &mut Cache<'_>,
    ) -> Vec<MeasuredPrefix> {
        let set = self.assemble(query.into(), 1, cache);
        let mut strs = Default::default();
        for m in set.iter() {
            self.trie.fill_results(&self.trie.nodes[m.node], &mut strs);
        }
        measure_results(strs, query)
    }

    /// Adds the strings prefixed by `node` to `result` until all have been added or the `requested` size has been reached
    ///
    /// Returns whether the `requested` size has been reached
    fn fill_results_limit(
        &self,
        node: &Node<UUU, SSS>,
        result: &mut HashSet<TreeString<'stored>>,
        requested: usize,
    ) -> bool {
        if requested == 0 {
            return true;
        }
        debug_assert_ne!(result.len(), requested);

        for string_index in node.string_range.clone() {
            result.insert(self.trie.strings[string_index as usize].clone());
            if result.len() >= requested {
                return true;
            }
        }
        debug_assert_ne!(result.len(), requested);
        false
    }
    /// Applies the `visitor` function to all descendants in the inverted index at `depth` and `character` of `matching.node`
    fn traverse_inverted_index<'a, VisitorFn>(
        &'a self,
        matching: Matching<UUU>,
        depth: usize,
        character: char,
        mut visitor: VisitorFn,
    ) where
        VisitorFn: FnMut(NodeID, &'a Node<UUU, SSS>),
    {
        let node = &self.trie.nodes[matching.node];
        if let Some(nodes) = self.inverted_index.get(depth, character) {
            // get the index where the first descendant of the node would be
            let start = nodes.partition_point(|&id| id < node.first_descendant_id() as SSS);

            // get the index of where the first node after all descendants would be
            let end = nodes.partition_point(|&id| id < node.descendant_range.end);

            let descendant_ids = &nodes[start..end];

            for &descendant_id in descendant_ids {
                visitor(
                    descendant_id.try_into().unwrap(),
                    &self.trie.nodes[descendant_id as usize],
                );
            }
        }
    }
    /// Extending the set from P(i-1,b) to P(i,b)
    fn first_deducing<'c>(
        &'c self,
        set: &MatchingSet<UUU>,
        character: char,
        query_len: usize, // i
        b: usize,
    ) -> MatchingSet<u8> {
        let mut delta = MatchingSet::default();
        let mut edit_distances = HashMap::<usize, UUU>::new(); // Node ID to ED(q,n)
        for m1 in set.iter() {
            if m1.edit_distance <= b as UUU
                && m1.query_prefix_len >= (query_len - 1 - b) as UUU
                && m1.query_prefix_len <= (query_len - 1) as UUU
            // m1.i >= i-1
            {
                let m1_node = &self.trie.nodes[m1.node];
                let m1_depth = m1_node.depth as usize;
                for depth in m1_depth + 1..=min(m1_depth + b + 1, self.inverted_index.max_depth()) {
                    // theorem ed-delta
                    if query_len.abs_diff(depth) <= b {
                        self.traverse_inverted_index(
                            m1.clone(),
                            depth,
                            character,
                            |id, descendant| {
                                // the depth of a node is equal to the length of its associated prefix
                                let ded = m1.deduced_edit_distance(
                                    query_len - 1,
                                    depth.saturating_sub(1) as usize,
                                    &self.trie.nodes,
                                );
                                let ded = ded as UUU;
                                let n2 = descendant.id();
                                if ded <= b as UUU {
                                    if let Some(edit_distance) = edit_distances.get_mut(&n2) {
                                        *edit_distance = min(*edit_distance, ded);
                                    } else {
                                        edit_distances.insert(n2, ded);
                                    }
                                }
                            },
                        );
                    }
                }
            }
        }
        for (node_id, edit_distance) in edit_distances {
            let query_prefix_len = query_len as UUU;
            let node = node_id;
            let matching = Matching::<UUU> {
                query_prefix_len,
                node,
                edit_distance,
            };
            delta.insert(matching);
        }
        delta
    }
    /// Expand the set from P(i,b-1) to P(i,b).
    /// Returns the delta, ie. P4
    fn second_deducing<'a, 'b: 'a>(
        &'a self,
        set: &'a MatchingSet<UUU>,
        query: &[char],
        query_len: usize,
        b: usize,
    ) -> MatchingSet<UUU>
    where
        'stored: 'b,
    {
        let mut set_p4: MatchingSet<UUU> = Default::default();
        let mut per_matching = |matching: Matching<UUU>| -> () {
            let last_depth = min(
                self.trie.nodes[matching.node].depth as usize + b - matching.edit_distance as usize
                    + 1,
                self.inverted_index.max_depth(),
            );
            let last_query_prefix_len = min(
                matching.query_prefix_len as usize + b - matching.edit_distance as usize + 1, // k+1+i_1
                query_len,
            );

            let mut check =
                |node: NodeID, descendant: &Node<UUU, SSS>, query_prefix_len: usize| -> () {
                    // m not in P_2 for any ed
                    if !set.contains(query_prefix_len as UUU, node)
                        && matching.deduced_edit_distance(
                            query_prefix_len - 1,
                            descendant.depth as usize - 1,
                            &self.trie.nodes,
                        ) == b
                    {
                        let matching = Matching::<UUU> {
                            query_prefix_len: query_prefix_len as UUU,
                            node,
                            edit_distance: b as UUU,
                        };
                        set_p4.insert(matching);
                    }
                };

            for query_prefix_len in matching.query_prefix_len as usize + 1..last_query_prefix_len {
                let character = query[query_prefix_len - 1];
                // theorem ed-delta
                if query_prefix_len.abs_diff(last_depth) <= b {
                    self.traverse_inverted_index(
                        matching.clone(),
                        last_depth,
                        character,
                        |id, descendant| check(id, descendant, query_prefix_len),
                    );
                }
            }

            let last_character = query[last_query_prefix_len - 1]; // the index in paper starts from one.
            for depth in self.trie.nodes[matching.node].depth as usize + 1..last_depth {
                if last_query_prefix_len.abs_diff(depth) <= b {
                    self.traverse_inverted_index(
                        matching.clone(),
                        depth,
                        last_character,
                        |id, descendant| check(id, descendant, last_query_prefix_len),
                    );
                }
            }

            self.traverse_inverted_index(
                matching.clone(),
                last_query_prefix_len,
                last_character,
                |id, descendant| check(id, descendant, last_query_prefix_len),
            );
        };

        // Filter the input set to P(i,b-1)
        for m in set.iter() {
            if m.edit_distance <= b as UUU - 1 && m.query_prefix_len <= query_len as UUU {
                per_matching(m);
            }
        }

        set_p4
    }
}

fn measure_results(result: HashSet<Cow<'_, str>>, query: &str) -> Vec<MeasuredPrefix> {
    let mut result: Vec<MeasuredPrefix> = result
        .into_iter()
        .map(|string| MeasuredPrefix {
            string: string.to_string(),
            prefix_distance: levenshtein::prefix_edit_distance(query, TreeStringT::to_str(&string)),
        })
        .collect();

    result.sort();
    result
}

impl Autocompleter for Yoke<MetaAutocompleter<'static>, Vec<String>> {
    fn threshold_topk(
        &self,
        query: &str,
        requested: usize,
        max_threshold: usize,
    ) -> Vec<MeasuredPrefix> {
        unimplemented!()
    }
}

impl FromStrings for Yoke<MetaAutocompleter<'static>, Vec<String>> {
    fn from_strings(strings: &[&str]) -> Self {
        let cart = strings.iter().map(|&s| s.to_string()).collect();
        Yoke::attach_to_cart(cart, |strings| {
            let cows: Vec<_> = strings.iter().map(Into::into).collect();
            MetaAutocompleter::new(cows.len(), cows)
        })
    }
}
