//! Boosting in Parakeet's greedy TDT search (docs/vocabulary.md, method b): the listed words
//! as a prefix tree of the model's word pieces with Aho-Corasick failure links, after k2/icefall
//! `icefall/context_graph.py` (`ContextGraph`) and sherpa-onnx `csrc/context-graph.cc`. A token
//! gets the bonus when it continues the current match or starts a listed word (an arc from the
//! current node or from any node on its failure chain, the root included). Greedy search
//! commits every token, so a bonus collected by a match that is then abandoned is never taken
//! back (icefall's beam search subtracts it on the failure arc).

use std::collections::{BTreeMap, HashMap, VecDeque};

use earshot_core::asr::{Vocab, WORD_START};
use earshot_onnx::parakeet::TokenBias;

/// The word pieces of `word` with the word-start mark, by longest match from the left;
/// `None` when some text matches no piece.
pub fn tokenise(word: &str, pieces: &HashMap<String, usize>) -> Option<Vec<usize>> {
    let text = format!("{WORD_START}{word}");
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let start = chars[at].0;
        let mut found = None;
        for end in (at + 1..=chars.len()).rev() {
            let stop = chars.get(end).map_or(text.len(), |c| c.0);
            if let Some(&id) = pieces.get(&text[start..stop]) {
                found = Some((id, end));
                break;
            }
        }
        let (id, end) = found?;
        out.push(id);
        at = end;
    }
    Some(out)
}

/// Every piece of `vocab` but blank, by text.
pub fn pieces(vocab: &Vocab) -> HashMap<String, usize> {
    (0..vocab.len())
        .filter(|&id| id != vocab.blank())
        .filter_map(|id| vocab.token(id).map(|t| (t.to_owned(), id)))
        .collect()
}

/// The prefix tree with failure links. Node 0 is the root.
#[derive(Debug, Clone)]
pub struct ContextGraph {
    goto: Vec<BTreeMap<usize, usize>>,
    fail: Vec<usize>,
    /// Per node: the tokens that get the bonus there (arcs of the node and of its failure
    /// chain), sorted and without repeats.
    boosted: Vec<Vec<usize>>,
}

impl ContextGraph {
    /// The graph of `sequences` (empty ones ignored).
    pub fn new(sequences: &[Vec<usize>]) -> Self {
        let mut goto: Vec<BTreeMap<usize, usize>> = vec![BTreeMap::new()];
        for seq in sequences {
            let mut node = 0;
            for &t in seq {
                node = match goto[node].get(&t) {
                    Some(&n) => n,
                    None => {
                        goto.push(BTreeMap::new());
                        let n = goto.len() - 1;
                        goto[node].insert(t, n);
                        n
                    }
                };
            }
        }
        // Failure links breadth first (Aho and Corasick 1975).
        let mut fail = vec![0; goto.len()];
        let mut queue: VecDeque<usize> = goto[0].values().copied().collect();
        while let Some(node) = queue.pop_front() {
            let arcs: Vec<(usize, usize)> = goto[node].iter().map(|(&t, &n)| (t, n)).collect();
            for (t, child) in arcs {
                let mut f = fail[node];
                while f != 0 && !goto[f].contains_key(&t) {
                    f = fail[f];
                }
                fail[child] = goto[f]
                    .get(&t)
                    .copied()
                    .filter(|&n| n != child)
                    .unwrap_or(0);
                queue.push_back(child);
            }
        }
        let boosted = (0..goto.len())
            .map(|node| {
                let mut tokens: Vec<usize> = Vec::new();
                let mut n = node;
                loop {
                    tokens.extend(goto[n].keys());
                    if n == 0 {
                        break;
                    }
                    n = fail[n];
                }
                tokens.sort_unstable();
                tokens.dedup();
                tokens
            })
            .collect();
        Self {
            goto,
            fail,
            boosted,
        }
    }

    /// The node after `token` from `node`.
    pub fn next(&self, node: usize, token: usize) -> usize {
        let mut n = node;
        loop {
            if let Some(&m) = self.goto[n].get(&token) {
                return m;
            }
            if n == 0 {
                return 0;
            }
            n = self.fail[n];
        }
    }

    /// The tokens that get the bonus at `node`.
    pub fn boosted(&self, node: usize) -> &[usize] {
        &self.boosted[node]
    }

    /// Number of nodes, the root included.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.goto.len()
    }
}

/// The two forms of boosting (docs/vocabulary.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Boosted tokens compete with blank.
    Open,
    /// When the unboosted best token is blank, nothing changes.
    Gated,
}

/// A [`TokenBias`] adding `beta` to the boosted tokens of the current node.
pub struct Boost<'g> {
    pub graph: &'g ContextGraph,
    pub beta: f32,
    pub gate: Gate,
    pub blank: usize,
    pub node: usize,
}

fn argmax(v: &[f32]) -> usize {
    // The decoder's own rule (`tdt::argmax`): the last of equal maxima.
    v.iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i)
}

impl TokenBias for Boost<'_> {
    fn start(&mut self) {
        self.node = 0;
    }

    fn adjust(&mut self, logits: &mut [f32]) {
        if self.gate == Gate::Gated && argmax(logits) == self.blank {
            return;
        }
        for &t in self.graph.boosted(self.node) {
            if let Some(v) = logits.get_mut(t) {
                *v += self.beta;
            }
        }
    }

    fn emitted(&mut self, token: usize) {
        self.node = self.graph.next(self.node, token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces_of(list: &[&str]) -> HashMap<String, usize> {
        list.iter()
            .enumerate()
            .map(|(i, p)| ((*p).to_owned(), i))
            .collect()
    }

    #[test]
    fn vocab_tokenise_takes_the_longest_piece_from_the_left() {
        let p = pieces_of(&["▁b", "▁bo", "zz", "z", "le", "l", "e", "o"]);
        assert_eq!(tokenise("bozzle", &p), Some(vec![1, 2, 4]));
        assert_eq!(tokenise("bozq", &p), None);
    }

    #[test]
    fn vocab_context_graph_boosts_continuations_and_starts() {
        // Words 1-2-3 and 2-4.
        let g = ContextGraph::new(&[vec![1, 2, 3], vec![2, 4], vec![]]);
        assert_eq!(g.boosted(0), &[1, 2]);
        let n1 = g.next(0, 1);
        // After 1: continue with 2, or start a word (1 or 2).
        assert_eq!(g.boosted(n1), &[1, 2]);
        let n12 = g.next(n1, 2);
        // After 1 2: continue with 3; the failure link to "2" offers 4; the root 1 and 2.
        assert_eq!(g.boosted(n12), &[1, 2, 3, 4]);
        // A token that continues nothing goes back to the root.
        assert_eq!(g.next(n12, 9), 0);
        // 1 2 4: the failure link to "2" takes the 4.
        let n124 = g.next(n12, 4);
        assert_eq!(n124, g.next(g.next(0, 2), 4));
    }

    #[test]
    fn vocab_boost_adds_the_bonus_and_the_gate_keeps_blank() {
        let g = ContextGraph::new(&[vec![1, 2]]);
        let mut b = Boost {
            graph: &g,
            beta: 2.0,
            gate: Gate::Open,
            blank: 3,
            node: 0,
        };
        b.start();
        let mut l = vec![0.0, 0.0, 0.0, 1.0];
        b.adjust(&mut l);
        assert_eq!(l, [0.0, 2.0, 0.0, 1.0]);
        // Gated: the best unboosted token is blank, so nothing changes.
        b.gate = Gate::Gated;
        let mut l = vec![0.0, 0.0, 0.0, 1.0];
        b.adjust(&mut l);
        assert_eq!(l, [0.0, 0.0, 0.0, 1.0]);
        // Gated with a word already winning: the bonus applies.
        let mut l = vec![1.5, 0.0, 0.0, 1.0];
        b.adjust(&mut l);
        assert_eq!(l, [1.5, 2.0, 0.0, 1.0]);
        b.emitted(1);
        let mut l = vec![1.5, 0.0, 0.0, 1.0];
        b.adjust(&mut l);
        assert_eq!(l, [1.5, 2.0, 2.0, 1.0]);
        b.start();
        assert_eq!(b.node, 0);
        assert_eq!(g.len(), 3);
    }
}
