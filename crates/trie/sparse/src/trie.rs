use crate::{SparseTrieError, SparseTrieResult};
use alloy_primitives::{hex, keccak256, map::HashMap, B256};
use alloy_rlp::Decodable;
use reth_tracing::tracing::debug;
use reth_trie::{
    prefix_set::{PrefixSet, PrefixSetMut},
    RlpNode,
};
use reth_trie_common::{
    BranchNodeRef, ExtensionNodeRef, LeafNodeRef, Nibbles, TrieMask, TrieNode, CHILD_INDEX_RANGE,
    EMPTY_ROOT_HASH,
};
use smallvec::SmallVec;
use std::{collections::HashSet, fmt};

/// Inner representation of the sparse trie.
/// Sparse trie is blind by default until nodes are revealed.
#[derive(PartialEq, Eq, Default, Debug)]
pub enum SparseTrie {
    /// None of the trie nodes are known.
    #[default]
    Blind,
    /// The trie nodes have been revealed.
    Revealed(RevealedSparseTrie),
}

impl SparseTrie {
    /// Creates new revealed empty trie.
    pub fn revealed_empty() -> Self {
        Self::Revealed(RevealedSparseTrie::default())
    }

    /// Returns `true` if the sparse trie has no revealed nodes.
    pub const fn is_blind(&self) -> bool {
        matches!(self, Self::Blind)
    }

    /// Returns mutable reference to revealed sparse trie if the trie is not blind.
    pub fn as_revealed_mut(&mut self) -> Option<&mut RevealedSparseTrie> {
        if let Self::Revealed(revealed) = self {
            Some(revealed)
        } else {
            None
        }
    }

    /// Reveals the root node if the trie is blinded.
    ///
    /// # Returns
    ///
    /// Mutable reference to [`RevealedSparseTrie`].
    pub fn reveal_root(&mut self, root: TrieNode) -> SparseTrieResult<&mut RevealedSparseTrie> {
        if self.is_blind() {
            *self = Self::Revealed(RevealedSparseTrie::from_root(root)?)
        }
        Ok(self.as_revealed_mut().unwrap())
    }

    /// Update the leaf node.
    pub fn update_leaf(&mut self, path: Nibbles, value: Vec<u8>) -> SparseTrieResult<()> {
        let revealed = self.as_revealed_mut().ok_or(SparseTrieError::Blind)?;
        revealed.update_leaf(path, value)?;
        Ok(())
    }

    /// Calculates and returns the trie root if the trie has been revealed.
    pub fn root(&mut self) -> Option<B256> {
        Some(self.as_revealed_mut()?.root())
    }
}

/// The representation of revealed sparse trie.
///
/// ## Invariants
///
/// - The root node is always present in `nodes` collection.
/// - Each leaf entry in `nodes` collection must have a corresponding entry in `values` collection.
///   The opposite is also true.
/// - All keys in `values` collection are full leaf paths.
#[derive(PartialEq, Eq)]
pub struct RevealedSparseTrie {
    /// All trie nodes.
    nodes: HashMap<Nibbles, SparseNode>,
    /// All leaf values.
    values: HashMap<Nibbles, Vec<u8>>,
    /// Prefix set.
    prefix_set: PrefixSetMut,
    /// Reusable buffer for RLP encoding of nodes.
    rlp_buf: Vec<u8>,
}

impl fmt::Debug for RevealedSparseTrie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevealedSparseTrie")
            .field("nodes", &self.nodes)
            .field("values", &self.values)
            .field("prefix_set", &self.prefix_set)
            .field("rlp_buf", &hex::encode(&self.rlp_buf))
            .finish()
    }
}

impl Default for RevealedSparseTrie {
    fn default() -> Self {
        Self {
            nodes: HashMap::from_iter([(Nibbles::default(), SparseNode::Empty)]),
            values: HashMap::default(),
            prefix_set: PrefixSetMut::default(),
            rlp_buf: Vec::new(),
        }
    }
}

impl RevealedSparseTrie {
    /// Create new revealed sparse trie from the given root node.
    pub fn from_root(node: TrieNode) -> SparseTrieResult<Self> {
        let mut this = Self {
            nodes: HashMap::default(),
            values: HashMap::default(),
            prefix_set: PrefixSetMut::default(),
            rlp_buf: Vec::new(),
        };
        this.reveal_node(Nibbles::default(), node)?;
        Ok(this)
    }

    /// Reveal the trie node only if it was not known already.
    pub fn reveal_node(&mut self, path: Nibbles, node: TrieNode) -> SparseTrieResult<()> {
        // TODO: revise all inserts to not overwrite existing entries
        match node {
            TrieNode::EmptyRoot => {
                debug_assert!(path.is_empty());
                self.nodes.insert(path, SparseNode::Empty);
            }
            TrieNode::Branch(branch) => {
                let mut stack_ptr = branch.as_ref().first_child_index();
                for idx in CHILD_INDEX_RANGE {
                    if branch.state_mask.is_bit_set(idx) {
                        let mut child_path = path.clone();
                        child_path.push_unchecked(idx);
                        self.reveal_node_or_hash(child_path, &branch.stack[stack_ptr])?;
                        stack_ptr += 1;
                    }
                }
                self.nodes
                    .insert(path, SparseNode::Branch { state_mask: branch.state_mask, hash: None });
            }
            TrieNode::Extension(ext) => {
                let mut child_path = path.clone();
                child_path.extend_from_slice_unchecked(&ext.key);
                self.reveal_node_or_hash(child_path, &ext.child)?;
                self.nodes.insert(path, SparseNode::Extension { key: ext.key, hash: None });
            }
            TrieNode::Leaf(leaf) => {
                let mut full = path.clone();
                full.extend_from_slice_unchecked(&leaf.key);
                self.values.insert(full, leaf.value);
                self.nodes.insert(path, SparseNode::new_leaf(leaf.key));
            }
        }

        Ok(())
    }

    fn reveal_node_or_hash(&mut self, path: Nibbles, child: &[u8]) -> SparseTrieResult<()> {
        if child.len() == B256::len_bytes() + 1 {
            // TODO: revise insert to not overwrite existing entries
            self.nodes.insert(path, SparseNode::Hash(B256::from_slice(&child[1..])));
            return Ok(())
        }

        self.reveal_node(path, TrieNode::decode(&mut &child[..])?)
    }

    /// Update the leaf node with provided value.
    pub fn update_leaf(&mut self, path: Nibbles, value: Vec<u8>) -> SparseTrieResult<()> {
        self.prefix_set.insert(path.clone());
        let existing = self.values.insert(path.clone(), value);
        if existing.is_some() {
            // trie structure unchanged, return immediately
            return Ok(())
        }

        let mut current = Nibbles::default();
        while let Some(node) = self.nodes.get_mut(&current) {
            match node {
                SparseNode::Empty => {
                    *node = SparseNode::new_leaf(path);
                    break
                }
                SparseNode::Hash(hash) => {
                    return Err(SparseTrieError::BlindedNode { path: current, hash: *hash })
                }
                SparseNode::Leaf { key: current_key, .. } => {
                    current.extend_from_slice_unchecked(current_key);

                    // this leaf is being updated
                    if current == path {
                        unreachable!("we already checked leaf presence in the beginning");
                    }

                    // find the common prefix
                    let common = current.common_prefix_length(&path);

                    // update existing node
                    let new_ext_key = current.slice(current.len() - current_key.len()..common);
                    *node = SparseNode::new_ext(new_ext_key);

                    // create a branch node and corresponding leaves
                    self.nodes.insert(
                        current.slice(..common),
                        SparseNode::new_split_branch(current[common], path[common]),
                    );
                    self.nodes.insert(
                        path.slice(..=common),
                        SparseNode::new_leaf(path.slice(common + 1..)),
                    );
                    self.nodes.insert(
                        current.slice(..=common),
                        SparseNode::new_leaf(current.slice(common + 1..)),
                    );

                    break;
                }
                SparseNode::Extension { key, .. } => {
                    current.extend_from_slice(key);
                    if !path.starts_with(&current) {
                        // find the common prefix
                        let common = current.common_prefix_length(&path);

                        *key = current.slice(current.len() - key.len()..common);

                        // create state mask for new branch node
                        // NOTE: this might overwrite the current extension node
                        let branch = SparseNode::new_split_branch(current[common], path[common]);
                        self.nodes.insert(current.slice(..common), branch);

                        // create new leaf
                        let new_leaf = SparseNode::new_leaf(path.slice(common + 1..));
                        self.nodes.insert(path.slice(..=common), new_leaf);

                        // recreate extension to previous child if needed
                        let key = current.slice(common + 1..);
                        if !key.is_empty() {
                            self.nodes.insert(current.slice(..=common), SparseNode::new_ext(key));
                        }

                        break;
                    }
                }
                SparseNode::Branch { state_mask, .. } => {
                    let nibble = path[current.len()];
                    current.push_unchecked(nibble);
                    if !state_mask.is_bit_set(nibble) {
                        state_mask.set_bit(nibble);
                        let new_leaf = SparseNode::new_leaf(path.slice(current.len()..));
                        self.nodes.insert(current, new_leaf);
                        break;
                    }
                }
            };
        }

        Ok(())
    }

    /// Remove leaf node from the trie.
    pub fn remove_leaf(&mut self, path: Nibbles) -> SparseTrieResult<()> {
        self.prefix_set.insert(path.clone());
        let existing = self.values.remove(&path);
        if existing.is_none() {
            // trie structure unchanged, return immediately
            return Ok(())
        }

        let mut removed_nodes = self.take_nodes_for_path(&path)?;
        debug!(target: "trie::sparse", ?path, ?removed_nodes, "Removed nodes for path");
        // Pop the first node from the stack which is the leaf node we want to remove.
        let mut child = removed_nodes.pop().expect("leaf exists");
        #[cfg(debug_assertions)]
        {
            let mut child_path = child.path.clone();
            let SparseNode::Leaf { key, .. } = &child.node else { panic!("expected leaf node") };
            child_path.extend_from_slice_unchecked(key);
            assert_eq!(child_path, path);
        }

        // If we don't have any other removed nodes, insert an empty node at the root.
        if removed_nodes.is_empty() {
            debug_assert!(self.nodes.is_empty());
            self.nodes.insert(Nibbles::default(), SparseNode::Empty);

            return Ok(())
        }

        // Walk the stack of removed nodes from the back and re-insert them back into the trie,
        // adjusting the node type as needed.
        while let Some(removed_node) = removed_nodes.pop() {
            let removed_path = removed_node.path;

            let new_node = match &removed_node.node {
                SparseNode::Empty => return Err(SparseTrieError::Blind),
                SparseNode::Hash(hash) => {
                    return Err(SparseTrieError::BlindedNode { path: removed_path, hash: *hash })
                }
                SparseNode::Leaf { .. } => {
                    unreachable!("we already popped the leaf node")
                }
                SparseNode::Extension { key, .. } => {
                    // If the node is an extension node, we need to look at its child to see if we
                    // need to merge them.
                    match &child.node {
                        SparseNode::Empty => return Err(SparseTrieError::Blind),
                        SparseNode::Hash(hash) => {
                            return Err(SparseTrieError::BlindedNode {
                                path: child.path,
                                hash: *hash,
                            })
                        }
                        // For a leaf node, we collapse the extension node into a leaf node,
                        // extending the key. While it's impossible to encounter an extension node
                        // followed by a leaf node in a complete trie, it's possible here because we
                        // could have downgraded the extension node's child into a leaf node from
                        // another node type.
                        SparseNode::Leaf { key: leaf_key, .. } => {
                            self.nodes.remove(&child.path);

                            let mut new_key = key.clone();
                            new_key.extend_from_slice_unchecked(leaf_key);
                            SparseNode::new_leaf(new_key)
                        }
                        // For an extension node, we collapse them into one extension node,
                        // extending the key
                        SparseNode::Extension { key: extension_key, .. } => {
                            self.nodes.remove(&child.path);

                            let mut new_key = key.clone();
                            new_key.extend_from_slice_unchecked(extension_key);
                            SparseNode::new_ext(new_key)
                        }
                        // For a branch node, we just leave the extension node as-is.
                        SparseNode::Branch { .. } => removed_node.node,
                    }
                }
                SparseNode::Branch { mut state_mask, hash: _ } => {
                    // If the node is a branch node, we need to check the number of children left
                    // after deleting the child at the given nibble.

                    if let Some(removed_nibble) = removed_node.unset_branch_nibble {
                        state_mask.unset_bit(removed_nibble);
                    }

                    // If only one child is left set in the branch node, we need to collapse it.
                    if state_mask.count_bits() == 1 {
                        let child_nibble =
                            state_mask.first_set_bit_index().expect("state mask is not empty");

                        // Get full path of the only child node left.
                        let mut child_path = removed_path.clone();
                        child_path.push_unchecked(child_nibble);

                        // Remove the only child node.
                        let child = self.nodes.get(&child_path).unwrap();

                        debug!(target: "trie::sparse", ?removed_path, ?child_path, ?child, "Branch node has only one child");

                        let mut delete_child = false;
                        let new_node = match child {
                            SparseNode::Empty => return Err(SparseTrieError::Blind),
                            SparseNode::Hash(hash) => {
                                return Err(SparseTrieError::BlindedNode {
                                    path: child_path,
                                    hash: *hash,
                                })
                            }
                            // If the only child is a leaf node, we downgrade the branch node into a
                            // leaf node, prepending the nibble to the key, and delete the old
                            // child.
                            SparseNode::Leaf { key, .. } => {
                                delete_child = true;

                                let mut new_key = Nibbles::from_nibbles_unchecked([child_nibble]);
                                new_key.extend_from_slice_unchecked(key);
                                SparseNode::new_leaf(new_key)
                            }
                            // If the only child node is an extension node, we downgrade the branch
                            // node into an even longer extension node, prepending the nibble to the
                            // key, and delete the old child.
                            SparseNode::Extension { key, .. } => {
                                delete_child = true;

                                let mut new_key = Nibbles::from_nibbles_unchecked([child_nibble]);
                                new_key.extend_from_slice_unchecked(key);
                                SparseNode::new_ext(new_key)
                            }
                            // If the only child is a branch node, we downgrade the current branch
                            // node into a one-nibble extension node.
                            SparseNode::Branch { .. } => {
                                SparseNode::new_ext(Nibbles::from_nibbles_unchecked([child_nibble]))
                            }
                        };

                        if delete_child {
                            self.nodes.remove(&child_path);
                        }

                        new_node
                    }
                    // If more than one child is left set in the branch, we just re-insert it
                    // as-is.
                    else {
                        SparseNode::new_branch(state_mask)
                    }
                }
            };

            child = RemovedSparseNode {
                path: removed_path.clone(),
                node: new_node.clone(),
                unset_branch_nibble: None,
            };
            debug!(target: "trie::sparse", ?removed_path, ?new_node, "Re-inserting the node");
            self.nodes.insert(removed_path, new_node);
        }

        Ok(())
    }

    /// Traverse trie nodes down to the leaf node and collect all nodes along the path.
    fn take_nodes_for_path(&mut self, path: &Nibbles) -> SparseTrieResult<Vec<RemovedSparseNode>> {
        let mut current = Nibbles::default(); // Start traversal from the root
        let mut nodes = Vec::new(); // Collect traversed nodes

        while let Some(node) = self.nodes.remove(&current) {
            match &node {
                SparseNode::Empty => return Err(SparseTrieError::Blind),
                SparseNode::Hash(hash) => {
                    return Err(SparseTrieError::BlindedNode { path: current, hash: *hash })
                }
                SparseNode::Leaf { key: _key, .. } => {
                    // Leaf node is always the one that we're deleting, and no other leaf nodes can
                    // be found during traversal.

                    #[cfg(debug_assertions)]
                    {
                        let mut current = current.clone();
                        current.extend_from_slice_unchecked(_key);
                        assert_eq!(&current, path);
                    }

                    nodes.push(RemovedSparseNode {
                        path: current.clone(),
                        node,
                        unset_branch_nibble: None,
                    });
                    break
                }
                SparseNode::Extension { key, .. } => {
                    #[cfg(debug_assertions)]
                    {
                        let mut current = current.clone();
                        current.extend_from_slice_unchecked(key);
                        assert!(path.starts_with(&current));
                    }

                    let path = current.clone();
                    current.extend_from_slice_unchecked(key);
                    nodes.push(RemovedSparseNode { path, node, unset_branch_nibble: None });
                }
                SparseNode::Branch { state_mask, .. } => {
                    let nibble = path[current.len()];
                    debug_assert!(state_mask.is_bit_set(nibble));

                    // If the branch node has a child that is a leaf node that we're removing,
                    // we need to unset this nibble.
                    // Any other branch nodes will not require unsetting the nibble, because
                    // deleting one leaf node can not remove the whole path
                    // where the branch node is located.
                    let mut child_path =
                        Nibbles::from_nibbles([current.as_slice(), &[nibble]].concat());
                    let unset_branch_nibble = self
                        .nodes
                        .get(&child_path)
                        .map_or(false, move |node| match node {
                            SparseNode::Leaf { key, .. } => {
                                // Get full path of the leaf node
                                child_path.extend_from_slice_unchecked(key);
                                &child_path == path
                            }
                            _ => false,
                        })
                        .then_some(nibble);

                    nodes.push(RemovedSparseNode {
                        path: current.clone(),
                        node,
                        unset_branch_nibble,
                    });

                    current.push_unchecked(nibble);
                }
            }
        }

        Ok(nodes)
    }

    /// Return the root of the sparse trie.
    /// Updates all remaining dirty nodes before calculating the root.
    pub fn root(&mut self) -> B256 {
        // take the current prefix set.
        let mut prefix_set = std::mem::take(&mut self.prefix_set).freeze();
        let root_rlp = self.rlp_node(Nibbles::default(), &mut prefix_set);
        if let Some(root_hash) = root_rlp.as_hash() {
            root_hash
        } else {
            keccak256(root_rlp)
        }
    }

    /// Update node hashes only if their path exceeds the provided level.
    pub fn update_rlp_node_level(&mut self, min_len: usize) {
        let mut paths = Vec::from([Nibbles::default()]);
        let mut targets = HashSet::<Nibbles>::default();

        while let Some(mut path) = paths.pop() {
            match self.nodes.get(&path).unwrap() {
                SparseNode::Empty | SparseNode::Hash(_) => {}
                SparseNode::Leaf { .. } => {
                    targets.insert(path);
                }
                SparseNode::Extension { key, .. } => {
                    if path.len() >= min_len {
                        targets.insert(path);
                    } else {
                        path.extend_from_slice_unchecked(key);
                        paths.push(path);
                    }
                }
                SparseNode::Branch { state_mask, .. } => {
                    if path.len() >= min_len {
                        targets.insert(path);
                    } else {
                        for bit in CHILD_INDEX_RANGE {
                            if state_mask.is_bit_set(bit) {
                                let mut child_path = path.clone();
                                child_path.push_unchecked(bit);
                                paths.push(child_path);
                            }
                        }
                    }
                }
            }
        }

        let mut prefix_set = self.prefix_set.clone().freeze();
        for target in targets {
            self.rlp_node(target, &mut prefix_set);
        }
    }

    fn rlp_node(&mut self, path: Nibbles, prefix_set: &mut PrefixSet) -> RlpNode {
        // stack of paths we need rlp nodes for
        let mut path_stack = Vec::from([path]);
        // stack of rlp nodes
        let mut rlp_node_stack = Vec::<(Nibbles, RlpNode)>::new();
        // reusable branch child path
        let mut branch_child_buf = SmallVec::<[Nibbles; 16]>::new_const();
        // reusable branch value stack
        let mut branch_value_stack_buf = SmallVec::<[RlpNode; 16]>::new_const();

        'main: while let Some(path) = path_stack.pop() {
            let rlp_node = match self.nodes.get_mut(&path).unwrap() {
                SparseNode::Empty => RlpNode::word_rlp(&EMPTY_ROOT_HASH),
                SparseNode::Hash(hash) => RlpNode::word_rlp(hash),
                SparseNode::Leaf { key, hash } => {
                    self.rlp_buf.clear();
                    let mut path = path.clone();
                    path.extend_from_slice_unchecked(key);
                    if let Some(hash) = hash.filter(|_| !prefix_set.contains(&path)) {
                        RlpNode::word_rlp(&hash)
                    } else {
                        let value = self.values.get(&path).unwrap();
                        let rlp_node = LeafNodeRef { key, value }.rlp(&mut self.rlp_buf);
                        *hash = rlp_node.as_hash();
                        rlp_node
                    }
                }
                SparseNode::Extension { key, hash } => {
                    let mut child_path = path.clone();
                    child_path.extend_from_slice_unchecked(key);
                    if let Some(hash) = hash.filter(|_| !prefix_set.contains(&path)) {
                        RlpNode::word_rlp(&hash)
                    } else if rlp_node_stack.last().map_or(false, |e| e.0 == child_path) {
                        let (_, child) = rlp_node_stack.pop().unwrap();
                        self.rlp_buf.clear();
                        let rlp_node = ExtensionNodeRef::new(key, &child).rlp(&mut self.rlp_buf);
                        *hash = rlp_node.as_hash();
                        rlp_node
                    } else {
                        path_stack.extend([path, child_path]); // need to get rlp node for child first
                        continue
                    }
                }
                SparseNode::Branch { state_mask, hash } => {
                    if let Some(hash) = hash.filter(|_| !prefix_set.contains(&path)) {
                        rlp_node_stack.push((path, RlpNode::word_rlp(&hash)));
                        continue
                    }

                    branch_child_buf.clear();
                    for bit in CHILD_INDEX_RANGE {
                        if state_mask.is_bit_set(bit) {
                            let mut child = path.clone();
                            child.push_unchecked(bit);
                            branch_child_buf.push(child);
                        }
                    }

                    branch_value_stack_buf.clear();
                    for child_path in &branch_child_buf {
                        if rlp_node_stack.last().map_or(false, |e| &e.0 == child_path) {
                            let (_, child) = rlp_node_stack.pop().unwrap();
                            branch_value_stack_buf.push(child);
                        } else {
                            debug_assert!(branch_value_stack_buf.is_empty());
                            path_stack.push(path);
                            path_stack.extend(branch_child_buf.drain(..));
                            continue 'main
                        }
                    }

                    self.rlp_buf.clear();
                    let rlp_node = BranchNodeRef::new(&branch_value_stack_buf, *state_mask)
                        .rlp(&mut self.rlp_buf);
                    *hash = rlp_node.as_hash();
                    rlp_node
                }
            };
            rlp_node_stack.push((path, rlp_node));
        }

        rlp_node_stack.pop().unwrap().1
    }
}

/// Enum representing trie nodes in sparse trie.
#[derive(PartialEq, Eq, Clone, Debug)]
pub enum SparseNode {
    /// Empty trie node.
    Empty,
    /// The hash of the node that was not revealed.
    Hash(B256),
    /// Sparse leaf node with remaining key suffix.
    Leaf {
        /// Remaining key suffix for the leaf node.
        key: Nibbles,
        /// Pre-computed hash of the sparse node.
        /// Can be reused unless this trie path has been updated.
        hash: Option<B256>,
    },
    /// Sparse extension node with key.
    Extension {
        /// The key slice stored by this extension node.
        key: Nibbles,
        /// Pre-computed hash of the sparse node.
        /// Can be reused unless this trie path has been updated.
        hash: Option<B256>,
    },
    /// Sparse branch node with state mask.
    Branch {
        /// The bitmask representing children present in the branch node.
        state_mask: TrieMask,
        /// Pre-computed hash of the sparse node.
        /// Can be reused unless this trie path has been updated.
        hash: Option<B256>,
    },
}

impl SparseNode {
    /// Create new sparse node from [`TrieNode`].
    pub fn from_node(node: TrieNode) -> Self {
        match node {
            TrieNode::EmptyRoot => Self::Empty,
            TrieNode::Leaf(leaf) => Self::new_leaf(leaf.key),
            TrieNode::Extension(ext) => Self::new_ext(ext.key),
            TrieNode::Branch(branch) => Self::new_branch(branch.state_mask),
        }
    }

    /// Create new [`SparseNode::Branch`] from state mask.
    pub const fn new_branch(state_mask: TrieMask) -> Self {
        Self::Branch { state_mask, hash: None }
    }

    /// Create new [`SparseNode::Branch`] with two bits set.
    pub const fn new_split_branch(bit_a: u8, bit_b: u8) -> Self {
        let state_mask = TrieMask::new(
            // set bits for both children
            (1u16 << bit_a) | (1u16 << bit_b),
        );
        Self::Branch { state_mask, hash: None }
    }

    /// Create new [`SparseNode::Extension`] from the key slice.
    pub const fn new_ext(key: Nibbles) -> Self {
        Self::Extension { key, hash: None }
    }

    /// Create new [`SparseNode::Leaf`] from leaf key and value.
    pub const fn new_leaf(key: Nibbles) -> Self {
        Self::Leaf { key, hash: None }
    }
}

#[derive(Debug)]
struct RemovedSparseNode {
    path: Nibbles,
    node: SparseNode,
    unset_branch_nibble: Option<u8>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use alloy_primitives::U256;
    use itertools::Itertools;
    use proptest::prelude::*;
    use rand::seq::IteratorRandom;
    use reth_testing_utils::generators;
    use reth_trie::{BranchNode, ExtensionNode, LeafNode};
    use reth_trie_common::{
        proof::{ProofNodes, ProofRetainer},
        HashBuilder,
    };

    /// Calculate the state root by feeding the provided state to the hash builder and retaining the
    /// proofs for the provided targets.
    ///
    /// Returns the state root and the retained proof nodes.
    fn hash_builder_root_with_proofs<V: AsRef<[u8]>>(
        state: impl IntoIterator<Item = (Nibbles, V)>,
        proof_targets: impl IntoIterator<Item = Nibbles>,
    ) -> (B256, ProofNodes) {
        let mut hash_builder =
            HashBuilder::default().with_proof_retainer(ProofRetainer::from_iter(proof_targets));
        for (key, value) in state {
            hash_builder.add_leaf(key, value.as_ref());
        }
        (hash_builder.root(), hash_builder.take_proof_nodes())
    }

    /// Assert that the sparse trie nodes and the proof nodes from the hash builder are equal.
    fn assert_eq_sparse_trie_proof_nodes(
        sparse_trie: &RevealedSparseTrie,
        proof_nodes: ProofNodes,
    ) {
        let proof_nodes = proof_nodes
            .into_nodes_sorted()
            .into_iter()
            .map(|(path, node)| (path, TrieNode::decode(&mut node.as_ref()).unwrap()));

        let sparse_nodes = sparse_trie.nodes.iter().sorted_by_key(|(path, _)| *path);

        for ((proof_node_path, proof_node), (sparse_node_path, sparse_node)) in
            proof_nodes.zip(sparse_nodes)
        {
            assert_eq!(&proof_node_path, sparse_node_path);

            let equals = match (&proof_node, &sparse_node) {
                // Both nodes are empty
                (TrieNode::EmptyRoot, SparseNode::Empty) => true,
                // Both nodes are branches and have the same state mask
                (
                    TrieNode::Branch(BranchNode { state_mask: proof_state_mask, .. }),
                    SparseNode::Branch { state_mask: sparse_state_mask, .. },
                ) => proof_state_mask == sparse_state_mask,
                // Both nodes are extensions and have the same key
                (
                    TrieNode::Extension(ExtensionNode { key: proof_key, .. }),
                    SparseNode::Extension { key: sparse_key, .. },
                ) |
                // Both nodes are leaves and have the same key
                (
                    TrieNode::Leaf(LeafNode { key: proof_key, .. }),
                    SparseNode::Leaf { key: sparse_key, .. },
                ) => proof_key == sparse_key,
                // Empty and hash nodes are specific to the sparse trie, skip them
                (_, SparseNode::Empty | SparseNode::Hash(_)) => continue,
                _ => false,
            };
            assert!(equals, "proof node: {:?}, sparse node: {:?}", proof_node, sparse_node);
        }
    }

    #[test]
    fn sparse_trie_is_blind() {
        assert!(SparseTrie::default().is_blind());
        assert!(!SparseTrie::revealed_empty().is_blind());
    }

    #[test]
    fn sparse_trie_empty_update_one() {
        let path = Nibbles::unpack(B256::with_last_byte(42));
        let value = alloy_rlp::encode_fixed_size(&U256::from(1));

        let (hash_builder_root, hash_builder_proof_nodes) =
            hash_builder_root_with_proofs([(path.clone(), &value)], [path.clone()]);

        let mut sparse = RevealedSparseTrie::default();
        sparse.update_leaf(path, value.to_vec()).unwrap();
        let sparse_root = sparse.root();

        assert_eq!(sparse_root, hash_builder_root);
        assert_eq_sparse_trie_proof_nodes(&sparse, hash_builder_proof_nodes);
    }

    #[test]
    fn sparse_trie_empty_update_multiple_lower_nibbles() {
        let paths = (0..=16).map(|b| Nibbles::unpack(B256::with_last_byte(b))).collect::<Vec<_>>();
        let value = alloy_rlp::encode_fixed_size(&U256::from(1));

        let (hash_builder_root, hash_builder_proof_nodes) = hash_builder_root_with_proofs(
            paths.iter().cloned().zip(std::iter::repeat_with(|| value.clone())),
            paths.clone(),
        );

        let mut sparse = RevealedSparseTrie::default();
        for path in &paths {
            sparse.update_leaf(path.clone(), value.to_vec()).unwrap();
        }
        let sparse_root = sparse.root();

        assert_eq!(sparse_root, hash_builder_root);
        assert_eq_sparse_trie_proof_nodes(&sparse, hash_builder_proof_nodes);
    }

    #[test]
    fn sparse_trie_empty_update_multiple_upper_nibbles() {
        let paths = (239..=255).map(|b| Nibbles::unpack(B256::repeat_byte(b))).collect::<Vec<_>>();
        let value = alloy_rlp::encode_fixed_size(&U256::from(1));

        let (hash_builder_root, hash_builder_proof_nodes) = hash_builder_root_with_proofs(
            paths.iter().cloned().zip(std::iter::repeat_with(|| value.clone())),
            paths.clone(),
        );

        let mut sparse = RevealedSparseTrie::default();
        for path in &paths {
            sparse.update_leaf(path.clone(), value.to_vec()).unwrap();
        }
        let sparse_root = sparse.root();

        assert_eq!(sparse_root, hash_builder_root);
        assert_eq_sparse_trie_proof_nodes(&sparse, hash_builder_proof_nodes);
    }

    #[test]
    fn sparse_trie_empty_update_multiple() {
        let paths = (0..=255)
            .map(|b| {
                Nibbles::unpack(if b % 2 == 0 {
                    B256::repeat_byte(b)
                } else {
                    B256::with_last_byte(b)
                })
            })
            .collect::<Vec<_>>();
        let value = alloy_rlp::encode_fixed_size(&U256::from(1));

        let (hash_builder_root, hash_builder_proof_nodes) = hash_builder_root_with_proofs(
            paths.iter().sorted_unstable().cloned().zip(std::iter::repeat_with(|| value.clone())),
            paths.clone(),
        );

        let mut sparse = RevealedSparseTrie::default();
        for path in &paths {
            sparse.update_leaf(path.clone(), value.to_vec()).unwrap();
        }
        let sparse_root = sparse.root();

        assert_eq!(sparse_root, hash_builder_root);
        assert_eq_sparse_trie_proof_nodes(&sparse, hash_builder_proof_nodes);
    }

    #[test]
    fn sparse_trie_empty_update_repeated() {
        let paths = (0..=255).map(|b| Nibbles::unpack(B256::repeat_byte(b))).collect::<Vec<_>>();
        let old_value = alloy_rlp::encode_fixed_size(&U256::from(1));
        let new_value = alloy_rlp::encode_fixed_size(&U256::from(2));

        let (hash_builder_root, hash_builder_proof_nodes) = hash_builder_root_with_proofs(
            paths.iter().cloned().zip(std::iter::repeat_with(|| old_value.clone())),
            paths.clone(),
        );

        let mut sparse = RevealedSparseTrie::default();
        for path in &paths {
            sparse.update_leaf(path.clone(), old_value.to_vec()).unwrap();
        }
        let sparse_root = sparse.root();

        assert_eq!(sparse_root, hash_builder_root);
        assert_eq_sparse_trie_proof_nodes(&sparse, hash_builder_proof_nodes);

        let (hash_builder_root, hash_builder_proof_nodes) = hash_builder_root_with_proofs(
            paths.iter().cloned().zip(std::iter::repeat_with(|| new_value.clone())),
            paths.clone(),
        );

        for path in &paths {
            sparse.update_leaf(path.clone(), new_value.to_vec()).unwrap();
        }
        let sparse_root = sparse.root();

        assert_eq!(sparse_root, hash_builder_root);
        assert_eq_sparse_trie_proof_nodes(&sparse, hash_builder_proof_nodes);
    }

    #[test]
    fn sparse_trie_remove_leaf() {
        reth_tracing::init_test_tracing();

        let mut sparse = RevealedSparseTrie::default();

        let value = alloy_rlp::encode_fixed_size(&U256::ZERO).to_vec();

        sparse
            .update_leaf(Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x1]), value.clone())
            .unwrap();
        sparse
            .update_leaf(Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x3]), value.clone())
            .unwrap();
        sparse
            .update_leaf(Nibbles::from_nibbles([0x5, 0x2, 0x0, 0x1, 0x3]), value.clone())
            .unwrap();
        sparse
            .update_leaf(Nibbles::from_nibbles([0x5, 0x3, 0x1, 0x0, 0x2]), value.clone())
            .unwrap();
        sparse
            .update_leaf(Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x0, 0x2]), value.clone())
            .unwrap();
        sparse.update_leaf(Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x2, 0x0]), value).unwrap();

        // Extension (Key = 5)
        // └── Branch (Mask = 1011)
        //     ├── 0 -> Extension (Key = 23)
        //     │        └── Branch (Mask = 0101)
        //     │              ├── 1 -> Leaf (Key = 1, Path = 50231)
        //     │              └── 3 -> Leaf (Key = 3, Path = 50233)
        //     ├── 2 -> Leaf (Key = 013, Path = 52013)
        //     └── 3 -> Branch (Mask = 0101)
        //                ├── 1 -> Leaf (Key = 3102, Path = 53102)
        //                └── 3 -> Branch (Mask = 1010)
        //                       ├── 0 -> Leaf (Key = 3302, Path = 53302)
        //                       └── 2 -> Leaf (Key = 3320, Path = 53320)
        pretty_assertions::assert_eq!(
            sparse.nodes.clone().into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from_iter([
                (Nibbles::new(), SparseNode::new_ext(Nibbles::from_nibbles([0x5]))),
                (Nibbles::from_nibbles([0x5]), SparseNode::new_branch(0b1101.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x0]),
                    SparseNode::new_ext(Nibbles::from_nibbles([0x2, 0x3]))
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3]),
                    SparseNode::new_branch(0b1010.into())
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x1]),
                    SparseNode::new_leaf(Nibbles::new())
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x3]),
                    SparseNode::new_leaf(Nibbles::new())
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x2]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x0, 0x1, 0x3]))
                ),
                (Nibbles::from_nibbles([0x5, 0x3]), SparseNode::new_branch(0b1010.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x1]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x0, 0x2]))
                ),
                (Nibbles::from_nibbles([0x5, 0x3, 0x3]), SparseNode::new_branch(0b0101.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x2]))
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x2]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x0]))
                )
            ])
        );

        sparse.remove_leaf(Nibbles::from_nibbles([0x5, 0x2, 0x0, 0x1, 0x3])).unwrap();

        // Extension (Key = 5)
        // └── Branch (Mask = 1001)
        //     ├── 0 -> Extension (Key = 23)
        //     │        └── Branch (Mask = 0101)
        //     │              ├── 1 -> Leaf (Key = 0231, Path = 50231)
        //     │              └── 3 -> Leaf (Key = 0233, Path = 50233)
        //     └── 3 -> Branch (Mask = 0101)
        //                ├── 1 -> Leaf (Key = 3102, Path = 53102)
        //                └── 3 -> Branch (Mask = 1010)
        //                       ├── 0 -> Leaf (Key = 3302, Path = 53302)
        //                       └── 2 -> Leaf (Key = 3320, Path = 53320)
        pretty_assertions::assert_eq!(
            sparse.nodes.clone().into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from_iter([
                (Nibbles::new(), SparseNode::new_ext(Nibbles::from_nibbles([0x5]))),
                (Nibbles::from_nibbles([0x5]), SparseNode::new_branch(0b1001.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x0]),
                    SparseNode::new_ext(Nibbles::from_nibbles([0x2, 0x3]))
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3]),
                    SparseNode::new_branch(0b1010.into())
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x1]),
                    SparseNode::new_leaf(Nibbles::new())
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x3]),
                    SparseNode::new_leaf(Nibbles::new())
                ),
                (Nibbles::from_nibbles([0x5, 0x3]), SparseNode::new_branch(0b1010.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x1]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x0, 0x2]))
                ),
                (Nibbles::from_nibbles([0x5, 0x3, 0x3]), SparseNode::new_branch(0b0101.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x2]))
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x2]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x0]))
                )
            ])
        );

        sparse.remove_leaf(Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x1])).unwrap();

        // Extension (Key = 5)
        // └── Branch (Mask = 1001)
        //     ├── 0 -> Leaf (Key = 0233, Path = 50233)
        //     └── 3 -> Branch (Mask = 0101)
        //                ├── 1 -> Leaf (Key = 3102, Path = 53102)
        //                └── 3 -> Branch (Mask = 1010)
        //                       ├── 0 -> Leaf (Key = 3302, Path = 53302)
        //                       └── 2 -> Leaf (Key = 3320, Path = 53320)
        pretty_assertions::assert_eq!(
            sparse.nodes.clone().into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from_iter([
                (Nibbles::new(), SparseNode::new_ext(Nibbles::from_nibbles([0x5]))),
                (Nibbles::from_nibbles([0x5]), SparseNode::new_branch(0b1001.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x2, 0x3, 0x3]))
                ),
                (Nibbles::from_nibbles([0x5, 0x3]), SparseNode::new_branch(0b1010.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x1]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x0, 0x2]))
                ),
                (Nibbles::from_nibbles([0x5, 0x3, 0x3]), SparseNode::new_branch(0b0101.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x2]))
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x2]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x0]))
                )
            ])
        );

        sparse.remove_leaf(Nibbles::from_nibbles([0x5, 0x3, 0x1, 0x0, 0x2])).unwrap();

        // Extension (Key = 5)
        // └── Branch (Mask = 1001)
        //     ├── 0 -> Leaf (Key = 0233, Path = 50233)
        //     └── 3 -> Branch (Mask = 1010)
        //                ├── 0 -> Leaf (Key = 3302, Path = 53302)
        //                └── 2 -> Leaf (Key = 3320, Path = 53320)
        pretty_assertions::assert_eq!(
            sparse.nodes.clone().into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from_iter([
                (Nibbles::new(), SparseNode::new_ext(Nibbles::from_nibbles([0x5]))),
                (Nibbles::from_nibbles([0x5]), SparseNode::new_branch(0b1001.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x2, 0x3, 0x3]))
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x3]),
                    SparseNode::new_ext(Nibbles::from_nibbles([0x3]))
                ),
                (Nibbles::from_nibbles([0x5, 0x3, 0x3]), SparseNode::new_branch(0b0101.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x2]))
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x2]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x0]))
                )
            ])
        );

        sparse.remove_leaf(Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x2, 0x0])).unwrap();

        // Extension (Key = 5)
        // └── Branch (Mask = 1001)
        //     ├── 0 -> Leaf (Key = 0233, Path = 50233)
        //     └── 3 -> Leaf (Key = 3302, Path = 53302)
        pretty_assertions::assert_eq!(
            sparse.nodes.clone().into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from_iter([
                (Nibbles::new(), SparseNode::new_ext(Nibbles::from_nibbles([0x5]))),
                (Nibbles::from_nibbles([0x5]), SparseNode::new_branch(0b1001.into())),
                (
                    Nibbles::from_nibbles([0x5, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x2, 0x3, 0x3]))
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x3]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x3, 0x0, 0x2]))
                ),
            ])
        );

        sparse.remove_leaf(Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x3])).unwrap();

        // Leaf (Key = 53302)
        pretty_assertions::assert_eq!(
            sparse.nodes.clone().into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from_iter([(
                Nibbles::new(),
                SparseNode::new_leaf(Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x0, 0x2]))
            ),])
        );

        sparse.remove_leaf(Nibbles::from_nibbles([0x5, 0x3, 0x3, 0x0, 0x2])).unwrap();

        // Empty
        pretty_assertions::assert_eq!(
            sparse.nodes.clone().into_iter().collect::<BTreeMap<_, _>>(),
            BTreeMap::from_iter([(Nibbles::new(), SparseNode::Empty),])
        );
    }

    #[test]
    fn sparse_trie_fuzz() {
        proptest!(ProptestConfig::with_cases(10), |(updates: Vec<HashMap<B256, U256>>)| {
            let mut rng = generators::rng();

            let mut state = BTreeMap::default();
            let mut unpacked_state = BTreeMap::default();
            let mut sparse = RevealedSparseTrie::default();

            for update in updates {
                let keys_to_delete_len = update.len() / 2;

                let unpacked_update = update.iter().map(|(key, value)| (
                    Nibbles::unpack(key),
                    alloy_rlp::encode_fixed_size(value).to_vec()
                ));

                // Insert state updates into the sparse trie and calculate the root
                for (key, value) in unpacked_update.clone() {
                    sparse.update_leaf(key, value).unwrap();
                }
                let sparse_root = sparse.root();

                // Insert state updates into the hash builder and calculate the root
                unpacked_state.extend(unpacked_update);
                state.extend(update);
                let keys = state.keys().map(Nibbles::unpack).collect::<Vec<_>>();
                let (hash_builder_root, hash_builder_proof_nodes) = hash_builder_root_with_proofs(
                    unpacked_state.clone(),
                    keys,
                );

                // Assert that the sparse trie root matches the hash builder root
                assert_eq!(sparse_root, hash_builder_root);
                // Assert that the sparse trie nodes match the hash builder proof nodes
                assert_eq_sparse_trie_proof_nodes(&sparse, hash_builder_proof_nodes);

                // Delete some keys from both the hash builder and the sparse trie and check
                // that the sparse trie root still matches the hash builder root

                let keys_to_delete = state
                    .keys()
                    .choose_multiple(&mut rng, keys_to_delete_len)
                    .into_iter()
                    .copied()
                    .collect::<Vec<_>>();
                for key in keys_to_delete {
                    state.remove(&key).unwrap();
                    unpacked_state.remove(&Nibbles::unpack(key)).unwrap();
                    sparse.remove_leaf(Nibbles::unpack(key)).unwrap();
                }

                let sparse_root = sparse.root();

                let keys = state.keys().map(Nibbles::unpack).collect::<Vec<_>>();
                let (hash_builder_root, hash_builder_proof_nodes) = hash_builder_root_with_proofs(
                    unpacked_state.clone(),
                    keys,
                );

                // Assert that the sparse trie root matches the hash builder root
                assert_eq!(sparse_root, hash_builder_root);
                // Assert that the sparse trie nodes match the hash builder proof nodes
                assert_eq_sparse_trie_proof_nodes(&sparse, hash_builder_proof_nodes);
            }
        });
    }
}
