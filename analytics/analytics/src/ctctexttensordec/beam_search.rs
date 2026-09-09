// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
// Copyright (c) 2020 Oxford Nanopore Technologies, Ltd.
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0 AND MIT

// The beam search and suffix tree are derived from Oxford Nanopore Technologies'
// fast-ctc-decode, src/search.rs and src/tree.rs.

mod tree {
    use std::collections::HashMap;

    /// An element in a possible labelling.
    #[derive(Clone, Copy, Debug)]
    struct LabelNode {
        /// The index into the alphabet of this label.
        ///
        /// Note that blanks are not represented by a LabelNode - this is an actual label.
        label: usize,
        /// The index of the parent LabelNode.
        parent: i32,
    }

    /// A tree of labelling suffixes (partial labellings pinned to the end of the network output).
    #[derive(Debug)]
    pub(super) struct SuffixTree {
        // Invariants:
        //
        // nodes[i].parent < nodes.len() for all i
        // nodes[i].parent < 0 => nodes[i].parent == ROOT_NODE for all i
        // nodes.len() == children.len()
        //
        // For every ((parent, label), child) in children:
        //     nodes[child].label == label (child edge label matches child label)
        //     nodes[child].parent == parent (child's parent pointer is correct)
        nodes: Vec<LabelNode>,
        children: HashMap<(i32, usize), i32>,
    }

    pub(super) struct SuffixTreeIter<'a> {
        nodes: &'a Vec<LabelNode>,
        next: i32,
    }

    impl<'a> Iterator for SuffixTreeIter<'a> {
        type Item = usize;

        fn next(&mut self) -> Option<Self::Item> {
            if self.next >= 0 {
                // NB: we could use an unsafe deref here as we maintain the invariant that
                // next <= nodes.len()
                let node = &self.nodes[self.next as usize];
                self.next = node.parent;
                Some(node.label)
            } else {
                None
            }
        }
    }

    pub(super) const ROOT_NODE: i32 = -1;

    impl SuffixTree {
        pub(super) fn new() -> Self {
            Self {
                nodes: Vec::new(),
                children: HashMap::new(),
            }
        }

        pub(super) fn label(&self, node: i32) -> Option<usize> {
            if node >= 0 {
                Some(self.nodes[node as usize].label)
            } else {
                None
            }
        }

        pub(super) fn clear(&mut self) {
            self.nodes.clear();
            self.children.clear();
        }

        pub(super) fn add_node(&mut self, parent: i32, label: usize) -> i32 {
            assert!(self.nodes.len() < (i32::MAX as usize));

            let new_node_idx = self.nodes.len() as i32;
            assert!(
                self.children
                    .insert((parent, label), new_node_idx)
                    .is_none()
            );
            self.nodes.push(LabelNode { label, parent });
            new_node_idx
        }

        pub(super) fn get_child(&self, node: i32, label: usize) -> Option<i32> {
            self.children.get(&(node, label)).copied()
        }

        pub(super) fn iter_from(&self, node: i32) -> SuffixTreeIter<'_> {
            assert!((node as usize) < self.nodes.len());
            SuffixTreeIter {
                nodes: &self.nodes,
                next: node,
            }
        }
    }
}

use tree::{ROOT_NODE, SuffixTree};

#[derive(Debug, Clone, Copy)]
struct SearchPoint {
    node: i32,
    label_prob: f64,
    gap_prob: f64,
}

impl SearchPoint {
    fn probability(&self) -> f64 {
        log_add(self.label_prob, self.gap_prob)
    }
}

#[derive(Debug)]
pub(super) struct BeamSearch {
    beam_width: usize,
    top_k: usize,
    beam: Vec<SearchPoint>,
    next_beam: Vec<SearchPoint>,
    tree: SuffixTree,
    tokens: Vec<usize>,
}

impl BeamSearch {
    pub(super) fn new(beam_width: usize, top_k: usize) -> Self {
        Self {
            beam_width,
            top_k,
            beam: Vec::new(),
            next_beam: Vec::new(),
            tree: SuffixTree::new(),
            tokens: Vec::new(),
        }
    }

    pub(super) fn decode(
        &mut self,
        data: &[f32],
        num_classes: usize,
        blank_index: usize,
        logits: bool,
    ) -> (&[usize], f64) {
        self.beam.clear();
        self.next_beam.clear();
        self.tree.clear();
        self.tokens.clear();

        self.beam.push(SearchPoint {
            node: ROOT_NODE,
            label_prob: f64::NEG_INFINITY,
            gap_prob: 0.0,
        });

        for scores in data.chunks_exact(num_classes) {
            self.next_beam.clear();
            let candidates = top_k_tokens(scores, self.top_k, blank_index, logits);

            for &SearchPoint {
                node,
                label_prob,
                gap_prob,
            } in &self.beam
            {
                // Last label of the decoded prefix represented by this beam
                let tip_label = self.tree.label(node);

                if let Some(&(_, pr_blank)) = candidates.iter().find(|&&(i, _)| i == blank_index) {
                    // Blank does not extend the decoded prefix. Keep the same node and
                    // move the total prefix probability to gap_prob.
                    self.next_beam.push(SearchPoint {
                        node,
                        label_prob: f64::NEG_INFINITY,
                        gap_prob: log_add(label_prob, gap_prob) + pr_blank,
                    });
                }

                for &(class_index, pr_b) in &candidates {
                    // blank handled the above
                    if class_index == blank_index {
                        continue;
                    }

                    // label is class index excluding blank token index
                    let label = class_index - usize::from(class_index > blank_index);

                    if Some(label) == tip_label {
                        // Repeated label without a preceding blank collapses into the same
                        // decoded prefix, so keep the current node
                        self.next_beam.push(SearchPoint {
                            node,
                            label_prob: label_prob + pr_b,
                            gap_prob: f64::NEG_INFINITY,
                        });

                        // A repeated label extends the prefix only from a blank-ending path.
                        // Reuse the child if this prefix was already created, otherwise create it
                        // only if such a path exists
                        let new_node_idx = self.tree.get_child(node, label).or_else(|| {
                            // the node has no child for the label, then assign new child node
                            if gap_prob != f64::NEG_INFINITY {
                                Some(self.tree.add_node(node, label))
                            } else {
                                None
                            }
                        });

                        if let Some(idx) = new_node_idx {
                            self.next_beam.push(SearchPoint {
                                node: idx,
                                label_prob: gap_prob + pr_b,
                                gap_prob: f64::NEG_INFINITY,
                            });
                        }
                    } else {
                        // A different label always extends the prefix. Both label-ending and
                        // blank-ending paths can transition to the same new prefix
                        let new_node_idx = self
                            .tree
                            .get_child(node, label)
                            .unwrap_or_else(|| self.tree.add_node(node, label));

                        self.next_beam.push(SearchPoint {
                            node: new_node_idx,
                            label_prob: log_add(label_prob, gap_prob) + pr_b,
                            gap_prob: f64::NEG_INFINITY,
                        });
                    }
                }
            }

            std::mem::swap(&mut self.beam, &mut self.next_beam);

            const DELETE_MARKER: i32 = i32::MIN;
            self.beam.sort_by_key(|x| x.node);
            let mut last_key = DELETE_MARKER;
            let mut last_key_pos = 0;
            for i in 0..self.beam.len() {
                let beam_item = self.beam[i];

                // Merge paths that produced the same decoded prefix
                if beam_item.node == last_key {
                    self.beam[last_key_pos].label_prob =
                        log_add(self.beam[last_key_pos].label_prob, beam_item.label_prob);

                    self.beam[last_key_pos].gap_prob =
                        log_add(self.beam[last_key_pos].gap_prob, beam_item.gap_prob);
                    self.beam[i].node = DELETE_MARKER;
                } else {
                    last_key_pos = i;
                    last_key = beam_item.node;
                }
            }

            self.beam.retain(|x| x.node != DELETE_MARKER);
            self.beam
                .sort_unstable_by(|a, b| b.probability().total_cmp(&a.probability()));

            // Keep only the beam_width most probable prefixes
            self.beam.truncate(self.beam_width);
            if self.beam.is_empty() {
                return (&self.tokens, 0.0);
            }
        }

        if self.beam[0].node != ROOT_NODE {
            // trace the leaf -> root tree path and store the labels
            self.tokens.extend(
                self.tree
                    .iter_from(self.beam[0].node)
                    .map(|label| label + usize::from(label >= blank_index)),
            );
        }

        // iter_from() walks from leaf to root, so restore the decoded token order
        self.tokens.reverse();

        let confidence = if self.tokens.is_empty() {
            0.0
        } else {
            (self.beam[0].probability() / self.tokens.len() as f64).exp()
        };

        (&self.tokens, confidence)
    }
}

fn top_k_tokens(
    scores: &[f32],
    top_k: usize,
    blank_index: usize,
    logits: bool,
) -> Vec<(usize, f64)> {
    let log_sum = if logits {
        Some(log_sum_exp(scores))
    } else {
        None
    };

    let mut tokens = if let Some(log_sum) = log_sum {
        scores
            .iter()
            .copied()
            .enumerate()
            .map(|(index, score)| (index, score as f64 - log_sum))
            .collect::<Vec<_>>()
    } else {
        scores
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, score)| *score > 0.0)
            .map(|(index, score)| (index, (score as f64).ln()))
            .collect::<Vec<_>>()
    };

    if top_k != 0 && tokens.len() > top_k {
        tokens.select_nth_unstable_by(top_k, |a, b| b.1.total_cmp(&a.1));
        tokens.truncate(top_k);

        // Blank tokens are required for CTC decoding even when the blank
        // class is outside the top-K candidates. Re-add it if needed
        if !tokens.iter().any(|(index, _)| *index == blank_index) {
            let score = scores[blank_index];

            if let Some(log_sum) = log_sum {
                tokens.push((blank_index, score as f64 - log_sum));
            } else if score > 0.0 {
                tokens.push((blank_index, (score as f64).ln()));
            }
        }
    }

    tokens
}

fn log_sum_exp(scores: &[f32]) -> f64 {
    let max = scores
        .iter()
        .copied()
        .max_by(|a, b| a.total_cmp(b))
        .unwrap() as f64;

    let sum = scores
        .iter()
        .map(|&score| ((score as f64) - max).exp())
        .sum::<f64>();

    max + sum.ln()
}

fn log_add(a: f64, b: f64) -> f64 {
    if a == f64::NEG_INFINITY {
        return b;
    }

    if b == f64::NEG_INFINITY {
        return a;
    }

    let max = a.max(b);
    let min = a.min(b);

    max + (min - max).exp().ln_1p()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Beam search case from upstream test_viterbi_blank_bounds in search.rs
    #[test]
    fn beam_search_blank_bounds() {
        let network_output = [
            [0.6f32, 0.2, 0.2],
            [0.6f32, 0.2, 0.2],
            [0.0f32, 0.4, 0.6],
            [0.0f32, 0.3, 0.7],
            [0.3f32, 0.3, 0.4],
            [0.4f32, 0.3, 0.3],
            [0.4f32, 0.3, 0.3],
            [0.3f32, 0.3, 0.4],
            [0.1f32, 0.4, 0.5],
            [0.1f32, 0.5, 0.4],
            [0.8f32, 0.1, 0.1],
            [0.1f32, 0.1, 0.8],
            [0.4f32, 0.3, 0.3],
        ];

        let mut search = BeamSearch::new(5, 0);
        let (tokens, _) = search.decode(network_output.as_flattened(), 3, 0, false);
        assert_eq!(tokens, &[2, 1, 2, 1, 2]);
    }

    #[test]
    fn nonzero_blank_index() {
        let mut search = BeamSearch::new(5, 0);
        let (tokens, confidence) = search.decode(&[1.0, 0.0, 0.0, 1.0, 1.0, 0.0], 2, 1, false);
        assert_eq!(tokens, &[0, 0]);
        assert_eq!(confidence, 1.0);
    }
}
