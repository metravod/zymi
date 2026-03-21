use std::collections::HashMap;

use petgraph::algo::{is_cyclic_directed, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

use super::node::{EdgeKind, PlanNode, WorkflowPlan};
use super::WorkflowError;

/// Directed acyclic graph of workflow nodes, backed by petgraph.
#[derive(Debug)]
pub struct WorkflowDag {
    graph: DiGraph<PlanNode, EdgeKind>,
    #[allow(dead_code)]
    index_map: HashMap<String, NodeIndex>,
}

impl WorkflowDag {
    /// Build the DAG from a planner-produced [`WorkflowPlan`].
    /// Validates uniqueness of node ids and absence of cycles.
    pub fn from_plan(plan: WorkflowPlan) -> Result<Self, WorkflowError> {
        let mut graph = DiGraph::new();
        let mut index_map = HashMap::new();

        // Add nodes
        for node in plan.nodes {
            let id = node.id.clone();
            let idx = graph.add_node(node);
            if index_map.insert(id.clone(), idx).is_some() {
                return Err(WorkflowError::InvalidDag(format!(
                    "duplicate node id: {id}"
                )));
            }
        }

        // Add edges
        for edge in plan.edges {
            let &from = index_map.get(&edge.from).ok_or_else(|| {
                WorkflowError::InvalidDag(format!(
                    "edge references unknown source node: {}",
                    edge.from
                ))
            })?;
            let &to = index_map.get(&edge.to).ok_or_else(|| {
                WorkflowError::InvalidDag(format!(
                    "edge references unknown target node: {}",
                    edge.to
                ))
            })?;
            graph.add_edge(from, to, edge.kind);
        }

        // Validate: no cycles
        if is_cyclic_directed(&graph) {
            return Err(WorkflowError::InvalidDag(
                "graph contains cycles".to_string(),
            ));
        }

        Ok(Self { graph, index_map })
    }

    /// Nodes in topological order (respecting dependencies).
    pub fn topological_order(&self) -> Result<Vec<NodeIndex>, WorkflowError> {
        toposort(&self.graph, None).map_err(|cycle| {
            WorkflowError::InvalidDag(format!("cycle detected at node {:?}", cycle.node_id()))
        })
    }

    /// Group nodes into execution levels.
    /// Nodes within the same level have no mutual dependencies and can run in parallel.
    pub fn execution_levels(&self) -> Result<Vec<Vec<NodeIndex>>, WorkflowError> {
        let topo = self.topological_order()?;
        let mut levels: Vec<Vec<NodeIndex>> = Vec::new();
        let mut node_level: HashMap<NodeIndex, usize> = HashMap::new();

        for &idx in &topo {
            let level = self
                .graph
                .neighbors_directed(idx, Direction::Incoming)
                .map(|dep| node_level.get(&dep).copied().unwrap_or(0) + 1)
                .max()
                .unwrap_or(0);

            node_level.insert(idx, level);

            if levels.len() <= level {
                levels.resize_with(level + 1, Vec::new);
            }
            levels[level].push(idx);
        }

        Ok(levels)
    }

    /// Reference to node data at the given index.
    pub fn node(&self, idx: NodeIndex) -> &PlanNode {
        &self.graph[idx]
    }

    /// Immediate dependency indices for a node (incoming neighbours).
    pub fn dependencies(&self, idx: NodeIndex) -> Vec<NodeIndex> {
        self.graph
            .neighbors_directed(idx, Direction::Incoming)
            .collect()
    }

    /// Total number of nodes.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::node::{NodeKind, PlanEdge, PlanNode};

    fn make_node(id: &str, kind: NodeKind) -> PlanNode {
        PlanNode {
            id: id.to_string(),
            kind,
            description: id.to_string(),
            tools: vec![],
            prompt: id.to_string(),
            tool_name: None,
            tool_arguments: None,
            runtime: None,
            install_command: None,
            mcp_source: None,
            mcp_server_name: None,
            max_retries: 0,
        }
    }

    #[test]
    fn linear_dag() {
        let plan = WorkflowPlan {
            nodes: vec![
                make_node("a", NodeKind::Research),
                make_node("b", NodeKind::Analysis),
                make_node("c", NodeKind::Synthesis),
            ],
            edges: vec![
                PlanEdge {
                    from: "a".into(),
                    to: "b".into(),
                    kind: EdgeKind::Data,
                },
                PlanEdge {
                    from: "b".into(),
                    to: "c".into(),
                    kind: EdgeKind::Data,
                },
            ],
        };

        let dag = WorkflowDag::from_plan(plan).unwrap();
        assert_eq!(dag.node_count(), 3);

        let levels = dag.execution_levels().unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].len(), 1);
        assert_eq!(levels[1].len(), 1);
        assert_eq!(levels[2].len(), 1);
    }

    #[test]
    fn parallel_dag() {
        //  a ──► c
        //  b ──► c
        let plan = WorkflowPlan {
            nodes: vec![
                make_node("a", NodeKind::Research),
                make_node("b", NodeKind::Research),
                make_node("c", NodeKind::Synthesis),
            ],
            edges: vec![
                PlanEdge {
                    from: "a".into(),
                    to: "c".into(),
                    kind: EdgeKind::Data,
                },
                PlanEdge {
                    from: "b".into(),
                    to: "c".into(),
                    kind: EdgeKind::Data,
                },
            ],
        };

        let dag = WorkflowDag::from_plan(plan).unwrap();
        let levels = dag.execution_levels().unwrap();
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0].len(), 2); // a, b in parallel
        assert_eq!(levels[1].len(), 1); // c
    }

    #[test]
    fn cycle_rejected() {
        let plan = WorkflowPlan {
            nodes: vec![
                make_node("a", NodeKind::Research),
                make_node("b", NodeKind::Analysis),
            ],
            edges: vec![
                PlanEdge {
                    from: "a".into(),
                    to: "b".into(),
                    kind: EdgeKind::Data,
                },
                PlanEdge {
                    from: "b".into(),
                    to: "a".into(),
                    kind: EdgeKind::Data,
                },
            ],
        };

        let err = WorkflowDag::from_plan(plan).unwrap_err();
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn duplicate_node_id_rejected() {
        let plan = WorkflowPlan {
            nodes: vec![
                make_node("a", NodeKind::Research),
                make_node("a", NodeKind::Analysis),
            ],
            edges: vec![],
        };

        let err = WorkflowDag::from_plan(plan).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn unknown_edge_target_rejected() {
        let plan = WorkflowPlan {
            nodes: vec![make_node("a", NodeKind::Research)],
            edges: vec![PlanEdge {
                from: "a".into(),
                to: "ghost".into(),
                kind: EdgeKind::Data,
            }],
        };

        let err = WorkflowDag::from_plan(plan).unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
