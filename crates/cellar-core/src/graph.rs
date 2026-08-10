//! Dependency and dependent graphs over an indexed bundle.
//!
//! The bundle's own `__d("Name", [deps], …)` declarations give the forward edges;
//! the indexer materializes the reverse ones. Both directions matter for different
//! questions: "what does this module need" traces an implementation downward, while
//! "who uses this" — the harder question to answer with grep, because minified call
//! sites do not spell the name — finds the feature a primitive belongs to.
//!
//! Traversal is breadth-first, so [`GraphNode::depth`] is the true shortest hop
//! distance from the nearest root, and a bounded `--depth` returns the closest
//! neighbourhood rather than an arbitrary slice of a deep chain.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::filter::FilterSet;
use crate::model::{BundleId, ModuleIndex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Follow `deps`: what these modules require.
    Deps,
    /// Follow `dependents`: what requires these modules.
    Dependents,
    /// Follow both, producing the connected neighbourhood.
    Both,
}

#[derive(Debug, Clone)]
pub struct GraphOptions {
    pub direction: Direction,
    /// Maximum hops from a root. `None` means unbounded.
    pub depth: Option<usize>,
    /// Stop after this many nodes. Truncation is always reported.
    pub max_nodes: usize,
    /// Include edges to names no module in this bundle defines. These are real —
    /// a bundle references modules split into other bundles — and hiding them
    /// would make a module look self-contained when it is not.
    pub include_external: bool,
    /// Detect cycles within the collected subgraph.
    pub detect_cycles: bool,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            direction: Direction::Deps,
            depth: Some(1),
            max_nodes: 2000,
            include_external: true,
            detect_cycles: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    pub name: String,
    /// Hops from the nearest root; roots are 0.
    pub depth: u32,
    /// False for a name referenced as a dependency but not defined in this bundle.
    pub present: bool,
    /// Path to the source, when the module is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Total dependencies this module declares (not just those inside the graph).
    pub dep_count: u32,
    /// Total modules that depend on it in the whole bundle.
    pub dependent_count: u32,
    /// False when the filter would have excluded this module. Nodes are not
    /// dropped for failing the filter — a filtered-out module can still be a real
    /// link in a chain — but they are marked so the caller can tell.
    pub in_filter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    /// The dependent module.
    pub from: String,
    /// The module it depends on.
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepGraph {
    pub bundle: BundleId,
    pub roots: Vec<String>,
    pub direction: Direction,
    /// Sorted by name.
    pub nodes: Vec<GraphNode>,
    /// Sorted by `(from, to)`.
    pub edges: Vec<GraphEdge>,
    /// Roots that are not in this bundle at all — distinguished from roots that
    /// exist but have no edges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_roots: Vec<String>,
    /// True when `maxNodes` cut the traversal short.
    pub truncated: bool,
    /// Cycles found within the collected subgraph, each as a node sequence whose
    /// first element repeats at the end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cycles: Vec<Vec<String>>,
}

/// Build a graph rooted at `roots`.
pub fn build(
    index: &ModuleIndex,
    roots: &[String],
    opts: &GraphOptions,
    filter: Option<&FilterSet>,
) -> DepGraph {
    let mut missing_roots: Vec<String> = roots
        .iter()
        .filter(|r| !index.contains(r))
        .cloned()
        .collect();
    missing_roots.sort();
    missing_roots.dedup();

    let mut depth_of: BTreeMap<String, u32> = BTreeMap::new();
    let mut edges: BTreeSet<GraphEdge> = BTreeSet::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    let mut truncated = false;

    for r in roots {
        if depth_of.insert(r.clone(), 0).is_none() {
            queue.push_back((r.clone(), 0));
        }
    }

    while let Some((name, depth)) = queue.pop_front() {
        if opts.depth.is_some_and(|max| depth as usize >= max) {
            continue;
        }
        let Some(entry) = index.get(&name) else {
            // An external name has no outgoing edges we can follow.
            continue;
        };

        let mut neighbours: Vec<(&str, bool)> = Vec::new();
        if matches!(opts.direction, Direction::Deps | Direction::Both) {
            neighbours.extend(entry.deps.iter().map(|d| (d.as_str(), true)));
        }
        if matches!(opts.direction, Direction::Dependents | Direction::Both) {
            neighbours.extend(entry.dependents.iter().map(|d| (d.as_str(), false)));
        }

        for (other, is_dep) in neighbours {
            let external = !index.contains(other);
            if external && !opts.include_external {
                continue;
            }
            // Edges point dependent -> dependency in both traversal directions, so
            // the emitted graph reads the same way regardless of how it was walked.
            let edge = if is_dep {
                GraphEdge {
                    from: name.clone(),
                    to: other.to_string(),
                }
            } else {
                GraphEdge {
                    from: other.to_string(),
                    to: name.clone(),
                }
            };
            edges.insert(edge);

            if !depth_of.contains_key(other) {
                if depth_of.len() >= opts.max_nodes {
                    truncated = true;
                    continue;
                }
                depth_of.insert(other.to_string(), depth + 1);
                queue.push_back((other.to_string(), depth + 1));
            }
        }
    }

    // Drop edges whose endpoints were cut by truncation, so the graph stays
    // internally consistent rather than referencing nodes it does not list.
    let edges: Vec<GraphEdge> = edges
        .into_iter()
        .filter(|e| depth_of.contains_key(&e.from) && depth_of.contains_key(&e.to))
        .collect();

    let nodes: Vec<GraphNode> = depth_of
        .iter()
        .map(|(name, depth)| {
            let entry = index.get(name);
            GraphNode {
                name: name.clone(),
                depth: *depth,
                present: entry.is_some(),
                file: entry.map(|e| e.file.clone()),
                dep_count: entry.map_or(0, |e| e.deps.len() as u32),
                dependent_count: entry.map_or(0, |e| e.dependents.len() as u32),
                in_filter: filter
                    .is_none_or(|f| matches!(f.classify(name).0, crate::filter::Verdict::Keep)),
            }
        })
        .collect();

    let cycles = if opts.detect_cycles {
        find_cycles(&nodes, &edges)
    } else {
        Vec::new()
    };

    let mut roots_sorted = roots.to_vec();
    roots_sorted.sort();
    roots_sorted.dedup();

    DepGraph {
        bundle: index.bundle,
        roots: roots_sorted,
        direction: opts.direction,
        nodes,
        edges,
        missing_roots,
        truncated,
        cycles,
    }
}

/// Iterative DFS cycle enumeration over the collected subgraph. Bounded by the
/// subgraph size, which `max_nodes` already caps.
fn find_cycles(nodes: &[GraphNode], edges: &[GraphEdge]) -> Vec<Vec<String>> {
    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for n in nodes {
        adj.entry(n.name.as_str()).or_default();
    }
    for e in edges {
        adj.entry(e.from.as_str()).or_default().push(e.to.as_str());
    }

    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Unseen,
        OnStack,
        Done,
    }

    let mut state: BTreeMap<&str, State> = adj.keys().map(|k| (*k, State::Unseen)).collect();
    let mut cycles: BTreeSet<Vec<String>> = BTreeSet::new();

    for start in adj.keys().copied() {
        if state[start] != State::Unseen {
            continue;
        }
        // (node, index of next neighbour to visit)
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        let mut path: Vec<&str> = vec![start];
        state.insert(start, State::OnStack);

        while let Some((node, next)) = stack.last_mut() {
            let node = *node;
            let neighbours = &adj[node];
            if *next >= neighbours.len() {
                state.insert(node, State::Done);
                stack.pop();
                path.pop();
                continue;
            }
            let neighbour = neighbours[*next];
            *next += 1;

            match state.get(neighbour).copied().unwrap_or(State::Done) {
                State::OnStack => {
                    if let Some(at) = path.iter().position(|n| *n == neighbour) {
                        let mut cycle: Vec<String> =
                            path[at..].iter().map(|s| s.to_string()).collect();
                        cycle.push(neighbour.to_string());
                        cycles.insert(canonical_cycle(cycle));
                    }
                }
                State::Unseen => {
                    state.insert(neighbour, State::OnStack);
                    stack.push((neighbour, 0));
                    path.push(neighbour);
                }
                State::Done => {}
            }
        }
    }

    cycles.into_iter().collect()
}

/// Rotate a cycle so it starts at its lexicographically smallest node, so the same
/// cycle discovered from two different entry points deduplicates to one entry.
fn canonical_cycle(mut cycle: Vec<String>) -> Vec<String> {
    cycle.pop(); // drop the repeated closing element
    if cycle.is_empty() {
        return cycle;
    }
    let min = cycle
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(i, _)| i)
        .unwrap_or(0);
    cycle.rotate_left(min);
    let first = cycle[0].clone();
    cycle.push(first);
    cycle
}

/// Render as Graphviz DOT.
pub fn to_dot(g: &DepGraph) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = String::from(
        "digraph cellar {\n  rankdir=LR;\n  node [shape=box, fontname=\"monospace\"];\n",
    );
    let roots: BTreeSet<&str> = g.roots.iter().map(String::as_str).collect();
    for n in &g.nodes {
        let mut attrs = vec![format!("label=\"{}\"", esc(&n.name))];
        if roots.contains(n.name.as_str()) {
            attrs.push("style=filled".into());
            attrs.push("fillcolor=\"#cde4ff\"".into());
        } else if !n.present {
            attrs.push("style=dashed".into());
            attrs.push("color=\"#999999\"".into());
        } else if !n.in_filter {
            attrs.push("color=\"#bbbbbb\"".into());
        }
        out.push_str(&format!("  \"{}\" [{}];\n", esc(&n.name), attrs.join(", ")));
    }
    for e in &g.edges {
        out.push_str(&format!("  \"{}\" -> \"{}\";\n", esc(&e.from), esc(&e.to)));
    }
    out.push_str("}\n");
    out
}

/// Render as a Mermaid flowchart, for pasting into Markdown.
pub fn to_mermaid(g: &DepGraph) -> String {
    // Mermaid node ids cannot hold arbitrary characters, so ids are positional and
    // the real name goes in the label.
    let ids: BTreeMap<&str, String> = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.name.as_str(), format!("n{i}")))
        .collect();
    let mut out = String::from("flowchart LR\n");
    for n in &g.nodes {
        let label = n.name.replace('"', "'");
        let shape = if n.present {
            format!("[\"{label}\"]")
        } else {
            format!("(\"{label}\")")
        };
        out.push_str(&format!("  {}{}\n", ids[n.name.as_str()], shape));
    }
    for e in &g.edges {
        if let (Some(a), Some(b)) = (ids.get(e.from.as_str()), ids.get(e.to.as_str())) {
            out.push_str(&format!("  {a} --> {b}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModuleEntry, Platform, SourceForm};

    fn entry(name: &str, deps: &[&str]) -> ModuleEntry {
        ModuleEntry {
            name: name.to_string(),
            file: format!("modules/{name}.js"),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            dependents: vec![],
            exports: vec![],
            functions: vec![],
            raw_sha256: String::new(),
            raw_len: 0,
            stored_len: 0,
            chunk: String::new(),
            form: SourceForm::Pretty,
            variants: vec![],
        }
    }

    fn index(entries: Vec<ModuleEntry>) -> ModuleIndex {
        let mut idx = ModuleIndex::new(BundleId::new(Platform::Whatsapp, 1), entries);
        let edges: Vec<(String, String)> = idx
            .modules
            .iter()
            .flat_map(|m| m.deps.iter().map(|d| (d.clone(), m.name.clone())))
            .collect();
        for (dep, dependent) in edges {
            if let Ok(i) = idx.modules.binary_search_by(|m| m.name.cmp(&dep)) {
                idx.modules[i].dependents.push(dependent);
            }
        }
        for m in &mut idx.modules {
            m.dependents.sort();
            m.dependents.dedup();
        }
        idx
    }

    fn names(g: &DepGraph) -> Vec<&str> {
        g.nodes.iter().map(|n| n.name.as_str()).collect()
    }

    #[test]
    fn depth_bound_is_shortest_hop_distance() {
        let idx = index(vec![
            entry("A", &["B"]),
            entry("B", &["C"]),
            entry("C", &[]),
        ]);
        let opts = GraphOptions {
            depth: Some(1),
            ..Default::default()
        };
        let g = build(&idx, &["A".into()], &opts, None);
        assert_eq!(names(&g), ["A", "B"]);

        let opts = GraphOptions {
            depth: Some(2),
            ..Default::default()
        };
        let g = build(&idx, &["A".into()], &opts, None);
        assert_eq!(names(&g), ["A", "B", "C"]);
    }

    #[test]
    fn dependents_direction_walks_the_reverse_edges() {
        let idx = index(vec![
            entry("Caller", &["Core"]),
            entry("Other", &["Core"]),
            entry("Core", &[]),
        ]);
        let opts = GraphOptions {
            direction: Direction::Dependents,
            depth: Some(1),
            ..Default::default()
        };
        let g = build(&idx, &["Core".into()], &opts, None);
        assert_eq!(names(&g), ["Caller", "Core", "Other"]);
        // Edges always read dependent -> dependency, whichever way we walked.
        assert!(g.edges.iter().all(|e| e.to == "Core"));
    }

    #[test]
    fn external_names_are_marked_not_hidden() {
        let idx = index(vec![entry("A", &["NotInBundle"])]);
        let g = build(&idx, &["A".into()], &GraphOptions::default(), None);
        let ext = g.nodes.iter().find(|n| n.name == "NotInBundle").unwrap();
        assert!(!ext.present);

        let opts = GraphOptions {
            include_external: false,
            ..Default::default()
        };
        let g = build(&idx, &["A".into()], &opts, None);
        assert_eq!(names(&g), ["A"]);
    }

    #[test]
    fn missing_roots_are_reported_separately_from_isolated_ones() {
        let idx = index(vec![entry("Lonely", &[])]);
        let g = build(
            &idx,
            &["Lonely".into(), "Ghost".into()],
            &GraphOptions::default(),
            None,
        );
        assert_eq!(g.missing_roots, ["Ghost"]);
        assert!(g.nodes.iter().any(|n| n.name == "Lonely" && n.present));
    }

    #[test]
    fn truncation_is_reported_and_leaves_no_dangling_edges() {
        let idx = index(vec![
            entry("A", &["B", "C", "D"]),
            entry("B", &[]),
            entry("C", &[]),
            entry("D", &[]),
        ]);
        let opts = GraphOptions {
            max_nodes: 2,
            ..Default::default()
        };
        let g = build(&idx, &["A".into()], &opts, None);
        assert!(g.truncated);
        let listed: BTreeSet<&str> = names(&g).into_iter().collect();
        for e in &g.edges {
            assert!(listed.contains(e.from.as_str()) && listed.contains(e.to.as_str()));
        }
    }

    #[test]
    fn cycles_are_found_and_deduplicated() {
        let idx = index(vec![
            entry("A", &["B"]),
            entry("B", &["C"]),
            entry("C", &["A"]),
        ]);
        let opts = GraphOptions {
            depth: None,
            detect_cycles: true,
            ..Default::default()
        };
        let g = build(&idx, &["A".into()], &opts, None);
        assert_eq!(g.cycles.len(), 1, "one cycle, not one per entry point");
        assert_eq!(g.cycles[0], ["A", "B", "C", "A"]);
    }

    #[test]
    fn renderers_emit_every_node_and_edge() {
        let idx = index(vec![entry("A", &["B"]), entry("B", &[])]);
        let g = build(&idx, &["A".into()], &GraphOptions::default(), None);
        let dot = to_dot(&g);
        assert!(dot.contains("\"A\" -> \"B\""));
        let mermaid = to_mermaid(&g);
        assert!(mermaid.starts_with("flowchart LR"));
        assert_eq!(mermaid.matches("-->").count(), g.edges.len());
    }
}
