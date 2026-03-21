use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

// ---------------------------------------------------------------------------
// Trace types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowTrace {
    pub id: String,
    pub user_message: String,
    pub timestamp: String,

    pub assessment: Option<AssessmentTrace>,
    pub planning: Option<PlanningTrace>,
    pub dag_structure: Option<DagTrace>,
    pub nodes: Vec<NodeTrace>,
    pub synthesis: Option<PhaseTrace>,

    pub total_duration_ms: u64,
    pub total_llm_calls: usize,
    pub total_tool_calls: usize,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssessmentTrace {
    pub score: u8,
    pub reasoning: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanningTrace {
    pub node_count: usize,
    pub edge_count: usize,
    pub raw_plan_json: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DagTrace {
    pub levels: Vec<Vec<String>>,
    pub total_nodes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeTrace {
    pub node_id: String,
    pub kind: String,
    pub description: String,
    pub level: usize,
    pub duration_ms: u64,
    pub iterations: usize,
    pub tool_calls: Vec<ToolCallTrace>,
    pub status: String,
    pub output_preview: String,
    pub attempts: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallTrace {
    pub tool_name: String,
    pub arguments_preview: String,
    pub result_preview: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseTrace {
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// Builder (accumulates data during execution)
// ---------------------------------------------------------------------------

pub struct TraceBuilder {
    pub id: String,
    pub user_message: String,
    pub start: Instant,

    pub assessment: Option<AssessmentTrace>,
    pub planning: Option<PlanningTrace>,
    pub dag_structure: Option<DagTrace>,
    pub nodes: Vec<NodeTrace>,
    pub synthesis: Option<PhaseTrace>,

    pub total_llm_calls: usize,
    pub total_tool_calls: usize,
}

impl TraceBuilder {
    pub fn new(user_message: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string()[..8].to_string(),
            user_message: user_message.to_string(),
            start: Instant::now(),
            assessment: None,
            planning: None,
            dag_structure: None,
            nodes: Vec::new(),
            synthesis: None,
            total_llm_calls: 0,
            total_tool_calls: 0,
        }
    }

    pub fn finish(self, outcome: &str) -> WorkflowTrace {
        WorkflowTrace {
            id: self.id,
            user_message: self.user_message,
            timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            assessment: self.assessment,
            planning: self.planning,
            dag_structure: self.dag_structure,
            nodes: self.nodes,
            synthesis: self.synthesis,
            total_duration_ms: self.start.elapsed().as_millis() as u64,
            total_llm_calls: self.total_llm_calls,
            total_tool_calls: self.total_tool_calls,
            outcome: outcome.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Node-level trace builder (used inside spawned tasks)
// ---------------------------------------------------------------------------

pub struct NodeTraceBuilder {
    pub node_id: String,
    pub kind: String,
    pub description: String,
    pub level: usize,
    pub start: Instant,
    pub iterations: usize,
    pub tool_calls: Vec<ToolCallTrace>,
    pub llm_calls: usize,
}

impl NodeTraceBuilder {
    pub fn new(node_id: &str, kind: &str, description: &str, level: usize) -> Self {
        Self {
            node_id: node_id.to_string(),
            kind: kind.to_string(),
            description: description.to_string(),
            level,
            start: Instant::now(),
            iterations: 0,
            tool_calls: Vec::new(),
            llm_calls: 0,
        }
    }

    pub fn record_tool_call(&mut self, name: &str, args: &str, result: &str, duration: Duration) {
        self.tool_calls.push(ToolCallTrace {
            tool_name: name.to_string(),
            arguments_preview: truncate(args, 200),
            result_preview: truncate(result, 300),
            duration_ms: duration.as_millis() as u64,
        });
    }

    pub fn finish(self, status: &str, output: &str) -> NodeTrace {
        self.finish_with_attempts(status, output, 1)
    }

    pub fn finish_with_attempts(self, status: &str, output: &str, attempts: usize) -> NodeTrace {
        NodeTrace {
            node_id: self.node_id,
            kind: self.kind,
            description: self.description,
            level: self.level,
            duration_ms: self.start.elapsed().as_millis() as u64,
            iterations: self.iterations,
            tool_calls: self.tool_calls,
            status: status.to_string(),
            output_preview: truncate(output, 500),
            attempts,
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

impl WorkflowTrace {
    /// Human-readable summary for CLI display.
    pub fn summary(&self) -> String {
        let mut s = String::new();

        s.push_str(&format!(
            "Workflow trace [{}] — {:.1}s total, {} LLM calls, {} tool calls\n",
            self.id,
            self.total_duration_ms as f64 / 1000.0,
            self.total_llm_calls,
            self.total_tool_calls,
        ));

        if let Some(ref a) = self.assessment {
            s.push_str(&format!(
                "  Assessment: {}/10 ({:.0}ms) — {}\n",
                a.score, a.duration_ms, a.reasoning
            ));
        }

        if let Some(ref p) = self.planning {
            s.push_str(&format!(
                "  Planning: {} nodes, {} edges ({:.0}ms)\n",
                p.node_count, p.edge_count, p.duration_ms
            ));
        }

        if let Some(ref d) = self.dag_structure {
            for (i, level) in d.levels.iter().enumerate() {
                let nodes = level.join(", ");
                let parallel = if level.len() > 1 { " [parallel]" } else { "" };
                s.push_str(&format!("  Level {i}: {nodes}{parallel}\n"));
            }
        }

        for node in &self.nodes {
            let tools_info = if node.tool_calls.is_empty() {
                String::new()
            } else {
                let names: Vec<&str> = node.tool_calls.iter().map(|t| t.tool_name.as_str()).collect();
                format!(", tools: {}", names.join("+"))
            };
            s.push_str(&format!(
                "  {} [{}] {} — {} ({:.0}ms, {} iter{})\n",
                node.node_id,
                node.kind,
                node.status,
                node.description,
                node.duration_ms,
                node.iterations,
                tools_info,
            ));
        }

        if let Some(ref syn) = self.synthesis {
            s.push_str(&format!("  Synthesis: {:.0}ms\n", syn.duration_ms));
        }

        s.push_str(&format!("  Outcome: {}\n", self.outcome));
        s
    }

    /// Save trace to a JSON file. Returns the file path.
    pub fn save(&self, memory_dir: &Path) -> Option<PathBuf> {
        let dir = memory_dir.join("workflow_traces");
        if std::fs::create_dir_all(&dir).is_err() {
            log::warn!("Failed to create workflow_traces directory");
            return None;
        }

        let filename = format!(
            "{}_{}.json",
            chrono::Local::now().format("%Y%m%d_%H%M%S"),
            self.id
        );
        let path = dir.join(&filename);

        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if std::fs::write(&path, json).is_ok() {
                    log::info!("Workflow trace saved to {}", path.display());
                    Some(path)
                } else {
                    log::warn!("Failed to write workflow trace");
                    None
                }
            }
            Err(e) => {
                log::warn!("Failed to serialize workflow trace: {e}");
                None
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}
