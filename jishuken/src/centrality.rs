//! Sybil-resistant, trust-weighted centrality.
//! Katz centrality where each node's voting contribution is scaled by its own
//! groundedness, so an ungrounded, freshly-ingested fact contributes nothing to
//! importance until it clears its own verification.
//! The control plane becomes robust to data-plane injection by construction.

use std::collections::HashMap;

/// A directed graph of facts. An edge `from -> to` means `from` supports /
/// depends on `to`, lending `to` importance.
#[derive(Debug, Default, Clone)]
pub struct FactGraph {
    nodes: Vec<String>,
    index: HashMap<String, usize>,
    edges: Vec<(usize, usize)>,
    /// Per-node groundedness weight in `[0,1]`.
    weight: Vec<f64>,
}

impl FactGraph {
    pub fn new() -> Self {
        FactGraph::default()
    }

    pub fn add_node(&mut self, id: impl Into<String>, trust_weight: f64) -> usize {
        let id = id.into();
        if let Some(&i) = self.index.get(&id) {
            self.weight[i] = trust_weight;
            return i;
        }
        let i = self.nodes.len();
        self.index.insert(id.clone(), i);
        self.nodes.push(id);
        self.weight.push(trust_weight.clamp(0.0, 1.0));
        i
    }

    pub fn add_edge(&mut self, from: &str, to: &str) {
        if let (Some(&f), Some(&t)) = (self.index.get(from), self.index.get(to)) {
            self.edges.push((f, t));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Trust-weighted Katz centrality by power iteration. A source node's
    /// contribution to its targets is scaled by that source's own groundedness
    /// weight, so ungrounded nodes (weight 0) cannot inflate anything.
    pub fn katz_trust_weighted(&self, alpha: f64, iters: usize) -> HashMap<String, f64> {
        let n = self.nodes.len();
        let mut c = vec![1.0f64; n];
        for _ in 0..iters {
            let mut next = vec![1.0f64; n]; // beta = 1 baseline
            for &(from, to) in &self.edges {
                next[to] += alpha * self.weight[from] * c[from];
            }
            c = next;
        }
        self.index
            .iter()
            .map(|(id, &i)| (id.clone(), c[i]))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ungrounded_sources_contribute_zero() {
        // Same topology, twice: a hub pointed at by two sources. When the
        // sources are ungrounded (weight 0) the hub gets only its baseline;
        // when they are grounded (weight 1) it gets a real boost.
        let mut grounded = FactGraph::new();
        grounded.add_node("hub", 1.0);
        grounded.add_node("s1", 1.0);
        grounded.add_node("s2", 1.0);
        grounded.add_edge("s1", "hub");
        grounded.add_edge("s2", "hub");

        let mut ungrounded = FactGraph::new();
        ungrounded.add_node("hub", 1.0);
        ungrounded.add_node("s1", 0.0);
        ungrounded.add_node("s2", 0.0);
        ungrounded.add_edge("s1", "hub");
        ungrounded.add_edge("s2", "hub");

        let g = grounded.katz_trust_weighted(0.5, 50);
        let u = ungrounded.katz_trust_weighted(0.5, 50);
        assert!(g["hub"] > u["hub"]);
        // The injection bought the attacker nothing: hub stays at baseline.
        assert!((u["hub"] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn more_grounded_support_means_more_central() {
        let mut g = FactGraph::new();
        g.add_node("popular", 1.0);
        g.add_node("lonely", 1.0);
        for i in 0..5 {
            let s = format!("s{i}");
            g.add_node(&s, 1.0);
            g.add_edge(&s, "popular");
        }
        g.add_node("only", 1.0);
        g.add_edge("only", "lonely");
        let c = g.katz_trust_weighted(0.3, 50);
        assert!(c["popular"] > c["lonely"]);
    }
}
