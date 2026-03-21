use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Node kinds & metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Gather information: web search, file reading, memory lookup
    Research,
    /// Generate or modify code, scripts, configurations
    CodeGen,
    /// Reason over collected data, compare options, draw conclusions
    Analysis,
    /// Direct invocation of a specific tool
    ToolCall,
    /// Create a script (Python/Bash/Node) for a task not covered by existing tools
    CreateTool,
    /// Connect an external MCP server
    ConnectMcp,
    /// Install a CLI tool or library
    InstallDep,
    /// Combine results from other nodes into the final answer
    Synthesis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRuntime {
    Python,
    Bash,
    Node,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Output of source is needed as input for target
    Data,
    /// Source must complete before target, but data is not passed
    Order,
}

/// How to obtain an MCP server (used by ConnectMcp nodes).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum McpSource {
    /// Install via `npx` / npm global
    Npm { package: String },
    /// Install via `pip` / `uvx`
    Pip { package: String },
    /// Docker image
    Docker { image: String },
    /// Remote HTTP/SSE endpoint
    Url { endpoint: String },
}

fn default_edge_kind() -> EdgeKind {
    EdgeKind::Data
}

// ---------------------------------------------------------------------------
// Execution status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants used as DAG execution progresses
pub enum NodeStatus {
    Pending,
    Running,
    Completed,
    Failed(String),
    Skipped,
}

#[derive(Debug, Clone)]
pub struct NodeResult {
    pub output: String,
    pub status: NodeStatus,
}

// ---------------------------------------------------------------------------
// Plan structures (LLM output → DAG input)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanNode {
    pub id: String,
    pub kind: NodeKind,
    pub description: String,
    #[serde(default)]
    pub tools: Vec<String>,
    pub prompt: String,

    // -- optional fields for specific node kinds --

    /// For `tool_call`: which tool to invoke
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// For `tool_call`: arguments JSON
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_arguments: Option<String>,
    /// For `create_tool`: script runtime
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ToolRuntime>,
    /// For `install_dep`: shell command to run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_command: Option<String>,
    /// For `connect_mcp`: how to obtain the server
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_source: Option<McpSource>,
    /// For `connect_mcp`: server name for mcp.json
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server_name: Option<String>,
    /// How many times to retry on failure (0-3, default 1).
    #[serde(default = "default_max_retries")]
    pub max_retries: u8,
}

fn default_max_retries() -> u8 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEdge {
    pub from: String,
    pub to: String,
    #[serde(default = "default_edge_kind")]
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowPlan {
    pub nodes: Vec<PlanNode>,
    pub edges: Vec<PlanEdge>,
}
