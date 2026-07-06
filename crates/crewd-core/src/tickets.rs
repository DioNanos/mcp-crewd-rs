//! Ask-ticket wait-for graph and deadlock prevention (SPEC §6.4).
//!
//! An edge `A → B` exists ONLY while `A` is blocked in an active `cell_await`
//! on an ask addressed to `B`. Opening an ask adds no edge; the deadlock check
//! happens at `cell_await` activation, never at ask creation (post-G1 model,
//! cross-review CR-B-02). Cycle → `E_WOULD_DEADLOCK`, the ask stays open and
//! answerable; only the await is refused.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::BusError;

/// Opaque guard for one activated await edge. Return it to
/// [`WaitForGraph::release`] when the await ends (answered, timeout, or error).
#[derive(Debug)]
pub struct AwaitGuard {
    from: String,
    to: String,
}

/// Directed wait-for graph over ACTIVE awaits only. Edge multiplicity is
/// ref-counted: parallel awaits on the same `(from, to)` pair stack.
#[derive(Default)]
pub struct WaitForGraph {
    edges: HashMap<(String, String), u32>,
}

impl WaitForGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently active distinct edges (test/introspection helper).
    pub fn edge_count(&self) -> usize {
        self.edges.values().filter(|&&c| c > 0).count()
    }

    /// Would activating `from → to` close a cycle? BFS from `to` over active
    /// edges looking for `from`.
    fn closes_cycle(&self, from: &str, to: &str) -> Option<Vec<String>> {
        if from == to {
            return Some(vec![from.to_string()]);
        }
        let mut queue: VecDeque<&str> = VecDeque::new();
        let mut visited: HashSet<&str> = HashSet::new();
        let mut parent: HashMap<&str, &str> = HashMap::new();
        queue.push_back(to);
        visited.insert(to);
        while let Some(node) = queue.pop_front() {
            for ((a, b), &count) in &self.edges {
                if count == 0 || a != node {
                    continue;
                }
                if b == from {
                    // Reconstruct the would-be cycle for the audit detail.
                    let mut cycle = vec![from.to_string(), to.to_string()];
                    let mut cur = node;
                    while cur != to {
                        cycle.push(cur.to_string());
                        cur = parent[cur];
                    }
                    return Some(cycle);
                }
                if visited.insert(b) {
                    parent.insert(b.as_str(), node);
                    queue.push_back(b);
                }
            }
        }
        None
    }

    /// Activate the await edge `from → to`. Rejects with `E_WOULD_DEADLOCK`
    /// if the edge would close a cycle over currently-active awaits.
    pub fn try_activate_await(&mut self, from: &str, to: &str) -> Result<AwaitGuard, BusError> {
        if let Some(cycle) = self.closes_cycle(from, to) {
            return Err(BusError::WouldDeadlock(format!(
                "await {from} -> {to} would close a cycle: {}",
                cycle.join(" -> ")
            )));
        }
        *self
            .edges
            .entry((from.to_string(), to.to_string()))
            .or_insert(0) += 1;
        Ok(AwaitGuard {
            from: from.to_string(),
            to: to.to_string(),
        })
    }

    /// Release one previously activated edge. Always call this when the await
    /// ends, on every path (answered, pending timeout, expired, error).
    pub fn release(&mut self, guard: AwaitGuard) {
        if let Some(count) = self.edges.get_mut(&(guard.from.clone(), guard.to.clone())) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.edges.remove(&(guard.from, guard.to));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_ask_without_await_adds_no_edge() {
        let g = WaitForGraph::new();
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn direct_cycle_rejected_at_await() {
        let mut g = WaitForGraph::new();
        let _ab = g.try_activate_await("a", "b").unwrap();
        assert_eq!(
            g.try_activate_await("b", "a").unwrap_err().code(),
            "E_WOULD_DEADLOCK"
        );
    }

    #[test]
    fn transitive_cycle_rejected() {
        let mut g = WaitForGraph::new();
        let _ab = g.try_activate_await("a", "b").unwrap();
        let _bc = g.try_activate_await("b", "c").unwrap();
        assert_eq!(
            g.try_activate_await("c", "a").unwrap_err().code(),
            "E_WOULD_DEADLOCK"
        );
    }

    #[test]
    fn released_edge_unblocks() {
        let mut g = WaitForGraph::new();
        let ab = g.try_activate_await("a", "b").unwrap();
        g.release(ab);
        assert!(
            g.try_activate_await("b", "a").is_ok(),
            "a's ask stays open but nobody is waiting: no cycle"
        );
    }

    #[test]
    fn parallel_awaits_same_pair_counted() {
        let mut g = WaitForGraph::new();
        let e1 = g.try_activate_await("a", "b").unwrap();
        let _e2 = g.try_activate_await("a", "b").unwrap();
        g.release(e1);
        assert_eq!(
            g.try_activate_await("b", "a").unwrap_err().code(),
            "E_WOULD_DEADLOCK",
            "an edge still active"
        );
    }

    #[test]
    fn self_await_is_a_cycle() {
        let mut g = WaitForGraph::new();
        assert_eq!(
            g.try_activate_await("a", "a").unwrap_err().code(),
            "E_WOULD_DEADLOCK"
        );
    }
}
