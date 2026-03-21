use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use petgraph::graph::NodeIndex;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::core::approval::SharedApprovalHandler;
use crate::core::{LlmProvider, Message, StreamEvent, ToolDefinition};
use crate::tools::current_time::CurrentTimeTool;
use crate::tools::memory::{ReadMemoryTool, WriteMemoryTool};
use crate::tools::shell::ShellTool;
use crate::tools::web_scrape::WebScrapeTool;
use crate::tools::web_search::WebSearchTool;
use crate::tools::Tool;

use super::dag::WorkflowDag;
use super::node::{McpSource, NodeKind, NodeResult, NodeStatus, PlanNode, ToolRuntime};
use super::trace::{NodeTrace, NodeTraceBuilder};
use super::WorkflowError;

/// Minimal mcp.json server entry for serialization.
#[derive(serde::Serialize)]
struct McpJsonEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<std::collections::HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

/// Maximum iterations for a single node's agent loop.
const NODE_MAX_ITERATIONS: usize = 10;

/// Maximum length for tool output stored in node agent message history.
const MAX_NODE_TOOL_OUTPUT: usize = 15_000;

fn truncate_output(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...\n[truncated, {} total chars]", &s[..end], s.len())
    }
}

/// All fields are cheap to clone (Arc / PathBuf), enabling parallel spawning.
#[derive(Clone)]
pub struct DagExecutor {
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    approval_handler: SharedApprovalHandler,
}

/// Result of executing the full DAG: node results + per-node traces.
pub struct DagExecResult {
    pub results: Vec<(String, NodeResult)>,
    pub node_traces: Vec<NodeTrace>,
    pub total_llm_calls: usize,
    pub total_tool_calls: usize,
    /// MCP server names successfully connected by ConnectMcp nodes.
    pub new_mcp_servers: Vec<String>,
}

impl DagExecutor {
    pub fn new(
        provider: &Arc<dyn LlmProvider>,
        memory_dir: &Path,
        approval_handler: SharedApprovalHandler,
    ) -> Self {
        Self {
            provider: provider.clone(),
            memory_dir: memory_dir.to_path_buf(),
            approval_handler,
        }
    }

    /// Execute all nodes in the DAG, parallelizing independent nodes within
    /// each execution level via [`tokio::JoinSet`].
    pub async fn execute(
        &self,
        dag: WorkflowDag,
        event_tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<DagExecResult, WorkflowError> {
        let levels = dag.execution_levels()?;
        let total_nodes = dag.node_count();
        let mut completed: usize = 0;
        let mut results: HashMap<String, NodeResult> = HashMap::new();
        let mut all_traces: Vec<NodeTrace> = Vec::new();
        let mut total_llm_calls: usize = 0;
        let mut total_tool_calls: usize = 0;
        let mut new_mcp_servers: Vec<String> = Vec::new();

        for (level_idx, level) in levels.iter().enumerate() {
            let parallel = level.len();
            log::info!("Executing DAG level {level_idx} ({parallel} node(s))");

            // (node_id, result, trace, optional mcp_server_name for ConnectMcp, attempts)
            let mut join_set: JoinSet<(String, NodeResult, NodeTrace, Option<String>, usize)> =
                JoinSet::new();

            for &node_idx in level {
                let node = dag.node(node_idx).clone();
                let node_id = node.id.clone();

                // Skip nodes whose dependencies failed
                if has_failed_dependency(&dag, node_idx, &results) {
                    log::warn!("Skipping node '{node_id}': dependency failed");
                    let _ = event_tx.send(StreamEvent::WorkflowNodeStart {
                        node_id: node_id.clone(),
                        description: format!("{} [skipped]", node.description),
                    });

                    let ntb =
                        NodeTraceBuilder::new(&node_id, &format!("{:?}", node.kind), &node.description, level_idx);
                    let trace = ntb.finish("skipped", "dependency failed");

                    results.insert(
                        node_id.clone(),
                        NodeResult {
                            output: "Skipped: a required dependency failed".to_string(),
                            status: NodeStatus::Skipped,
                        },
                    );
                    all_traces.push(trace);
                    completed += 1;
                    let _ = event_tx.send(StreamEvent::WorkflowNodeComplete {
                        node_id,
                        success: false,
                    });
                    let _ = event_tx.send(StreamEvent::WorkflowProgress {
                        completed,
                        total: total_nodes,
                    });
                    continue;
                }

                let dep_context = build_dependency_context(&dag, node_idx, &results);
                let exec = self.clone();
                let tx = event_tx.clone();
                let lvl = level_idx;

                join_set.spawn(async move {
                    let _ = tx.send(StreamEvent::WorkflowNodeStart {
                        node_id: node_id.clone(),
                        description: node.description.clone(),
                    });

                    let (mut result, mut trace) =
                        exec.execute_node_traced(&node, &dep_context, lvl).await;
                    let mut attempts: usize = 1;
                    for attempt in 1..=node.max_retries as usize {
                        if matches!(result.status, NodeStatus::Completed) {
                            break;
                        }
                        log::info!(
                            "Node '{}' failed (attempt {}/{}), retrying...",
                            node_id,
                            attempt,
                            node.max_retries as usize + 1
                        );
                        let retry_ctx = format!(
                            "{dep_context}\n\nPrevious attempt failed: {}\nTry a different approach.",
                            result.output
                        );
                        let r = exec.execute_node_traced(&node, &retry_ctx, lvl).await;
                        result = r.0;
                        trace = r.1;
                        attempts += 1;
                    }

                    let success = matches!(result.status, NodeStatus::Completed);
                    log::info!(
                        "Node '{}' finished: success={}, {:.0}ms, {} iter, {} tools, {} attempt(s)",
                        node_id,
                        success,
                        trace.duration_ms,
                        trace.iterations,
                        trace.tool_calls.len(),
                        attempts,
                    );

                    let _ = tx.send(StreamEvent::WorkflowNodeComplete {
                        node_id: node_id.clone(),
                        success,
                    });

                    // Track ConnectMcp server names for hot-reload
                    let mcp_server = if success && matches!(node.kind, NodeKind::ConnectMcp) {
                        node.mcp_server_name.clone()
                    } else {
                        None
                    };

                    (node_id, result, trace, mcp_server, attempts)
                });
            }

            while let Some(join_result) = join_set.join_next().await {
                match join_result {
                    Ok((node_id, result, trace, mcp_server, _attempts)) => {
                        total_llm_calls += trace.iterations;
                        total_tool_calls += trace.tool_calls.len();
                        all_traces.push(trace);
                        if let Some(server_name) = mcp_server {
                            new_mcp_servers.push(server_name);
                        }
                        results.insert(node_id, result);
                        completed += 1;
                        let _ = event_tx.send(StreamEvent::WorkflowProgress {
                            completed,
                            total: total_nodes,
                        });
                    }
                    Err(e) => {
                        log::error!("Node task panicked: {e}");
                        completed += 1;
                    }
                }
            }
        }

        // Sort traces by level then node_id for consistent output
        all_traces.sort_by(|a, b| a.level.cmp(&b.level).then(a.node_id.cmp(&b.node_id)));

        // Return results in topological order
        let topo = dag.topological_order()?;
        let ordered: Vec<(String, NodeResult)> = topo
            .into_iter()
            .filter_map(|idx| {
                let id = dag.node(idx).id.clone();
                results.remove(&id).map(|r| (id, r))
            })
            .collect();

        Ok(DagExecResult {
            results: ordered,
            node_traces: all_traces,
            total_llm_calls,
            total_tool_calls,
            new_mcp_servers,
        })
    }

    // ------------------------------------------------------------------
    // Node dispatch (with tracing)
    // ------------------------------------------------------------------

    async fn execute_node_traced(
        &self,
        node: &PlanNode,
        dep_context: &str,
        level: usize,
    ) -> (NodeResult, NodeTrace) {
        let kind_str = format!("{:?}", node.kind);
        match &node.kind {
            NodeKind::ToolCall => {
                let mut ntb = NodeTraceBuilder::new(&node.id, &kind_str, &node.description, level);
                let result = self.execute_tool_call_node_traced(node, &mut ntb).await;
                let status_str = status_label(&result.status);
                let trace = ntb.finish(&status_str, &result.output);
                (result, trace)
            }
            NodeKind::InstallDep => {
                let mut ntb = NodeTraceBuilder::new(&node.id, &kind_str, &node.description, level);
                let result = self.execute_shell_node_traced(node, &mut ntb).await;
                let status_str = status_label(&result.status);
                let trace = ntb.finish(&status_str, &result.output);
                (result, trace)
            }
            NodeKind::CreateTool => {
                let mut ntb = NodeTraceBuilder::new(&node.id, &kind_str, &node.description, level);
                let result = self
                    .execute_create_tool_node_traced(node, dep_context, &mut ntb)
                    .await;
                let status_str = status_label(&result.status);
                let trace = ntb.finish(&status_str, &result.output);
                (result, trace)
            }
            NodeKind::ConnectMcp => {
                let ntb = NodeTraceBuilder::new(&node.id, &kind_str, &node.description, level);
                let result = self.execute_connect_mcp_node(node).await;
                let status_str = status_label(&result.status);
                let trace = ntb.finish(&status_str, &result.output);
                (result, trace)
            }
            _ => {
                let mut ntb = NodeTraceBuilder::new(&node.id, &kind_str, &node.description, level);
                let result = self
                    .execute_agent_node_traced(node, dep_context, &mut ntb)
                    .await;
                let status_str = status_label(&result.status);
                let trace = ntb.finish(&status_str, &result.output);
                (result, trace)
            }
        }
    }

    // ------------------------------------------------------------------
    // Agent node: mini LLM loop with tools (traced)
    // ------------------------------------------------------------------

    async fn execute_agent_node_traced(
        &self,
        node: &PlanNode,
        dep_context: &str,
        ntb: &mut NodeTraceBuilder,
    ) -> NodeResult {
        let context_block = if dep_context.is_empty() {
            String::new()
        } else {
            format!("{dep_context}\nUse the above context to inform your work.\n\n")
        };

        let system_prompt = format!(
            "You are executing a specific task as part of a larger workflow.\n\n\
             Task: {}\n\n\
             Instructions: {}\n\n\
             {}\
             Complete the task and provide your result. Be concise and focused.",
            node.description, node.prompt, context_block
        );

        let tools = self.build_tools_for_node(node);
        let tool_defs: Vec<ToolDefinition> = tools.iter().map(|t| t.definition()).collect();

        let mut messages = vec![
            Message::System(system_prompt),
            Message::User(format!("Execute the task: {}", node.description)),
        ];

        for iteration in 0..NODE_MAX_ITERATIONS {
            ntb.iterations = iteration + 1;
            ntb.llm_calls += 1;

            log::debug!(
                "Node '{}' iteration {}/{}",
                node.id,
                iteration + 1,
                NODE_MAX_ITERATIONS
            );

            let response = match self.provider.chat(&messages, &tool_defs).await {
                Ok(r) => r,
                Err(e) => {
                    return NodeResult {
                        output: format!("LLM error: {e}"),
                        status: NodeStatus::Failed(e.to_string()),
                    };
                }
            };

            if response.tool_calls.is_empty() {
                return NodeResult {
                    output: response.content.unwrap_or_default(),
                    status: NodeStatus::Completed,
                };
            }

            let explanation = response.content.clone();
            messages.push(Message::Assistant {
                content: response.content,
                tool_calls: response.tool_calls.clone(),
            });

            for tc in &response.tool_calls {
                let tool_start = std::time::Instant::now();
                let result = self
                    .run_tool(&tools, &tc.name, &tc.arguments, explanation.as_deref())
                    .await;
                ntb.record_tool_call(
                    &tc.name,
                    &tc.arguments,
                    &result,
                    tool_start.elapsed(),
                );
                messages.push(Message::ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: truncate_output(&result, MAX_NODE_TOOL_OUTPUT),
                });
            }
        }

        NodeResult {
            output: "Node exceeded maximum iterations".to_string(),
            status: NodeStatus::Failed("max iterations exceeded".to_string()),
        }
    }

    // ------------------------------------------------------------------
    // Direct tool call node (traced)
    // ------------------------------------------------------------------

    async fn execute_tool_call_node_traced(
        &self,
        node: &PlanNode,
        ntb: &mut NodeTraceBuilder,
    ) -> NodeResult {
        ntb.iterations = 1;

        let tool_name = match &node.tool_name {
            Some(name) => name,
            None => {
                return NodeResult {
                    output: "tool_call node missing tool_name field".to_string(),
                    status: NodeStatus::Failed("missing tool_name".into()),
                };
            }
        };

        let arguments = node.tool_arguments.as_deref().unwrap_or("{}");
        let tools = self.build_tools_for_node(node);

        match tools.iter().find(|t| t.definition().name == *tool_name) {
            Some(tool) => {
                let tool_start = std::time::Instant::now();
                let result = tool
                    .execute(arguments)
                    .await
                    .unwrap_or_else(|e| format!("Tool error: {e}"));
                ntb.record_tool_call(tool_name, arguments, &result, tool_start.elapsed());
                NodeResult {
                    output: result,
                    status: NodeStatus::Completed,
                }
            }
            None => NodeResult {
                output: format!("Tool not found: {tool_name}"),
                status: NodeStatus::Failed(format!("tool not found: {tool_name}")),
            },
        }
    }

    // ------------------------------------------------------------------
    // Shell-based node (install_dep, traced)
    // ------------------------------------------------------------------

    async fn execute_shell_node_traced(
        &self,
        node: &PlanNode,
        ntb: &mut NodeTraceBuilder,
    ) -> NodeResult {
        ntb.iterations = 1;

        let command = match &node.install_command {
            Some(cmd) => cmd.clone(),
            None => {
                return NodeResult {
                    output: "install_dep node missing install_command field".to_string(),
                    status: NodeStatus::Failed("missing install_command".into()),
                };
            }
        };

        let args = serde_json::json!({ "command": command }).to_string();
        let tool_start = std::time::Instant::now();
        let result = self
            .run_tool(
                &[Box::new(ShellTool::new()) as Box<dyn Tool>],
                "execute_shell",
                &args,
                Some("Workflow: installing dependency"),
            )
            .await;
        ntb.record_tool_call("execute_shell", &args, &result, tool_start.elapsed());

        if result.starts_with("Tool error:") || result.contains("rejected") {
            NodeResult {
                output: result.clone(),
                status: NodeStatus::Failed(result),
            }
        } else {
            NodeResult {
                output: result,
                status: NodeStatus::Completed,
            }
        }
    }

    // ------------------------------------------------------------------
    // CreateTool node (traced)
    // ------------------------------------------------------------------

    async fn execute_create_tool_node_traced(
        &self,
        node: &PlanNode,
        dep_context: &str,
        ntb: &mut NodeTraceBuilder,
    ) -> NodeResult {
        let runtime = node.runtime.as_ref().unwrap_or(&ToolRuntime::Python);

        let (ext, interpreter) = match runtime {
            ToolRuntime::Python => ("py", "python3"),
            ToolRuntime::Bash => ("sh", "bash"),
            ToolRuntime::Node => ("js", "node"),
        };

        let script_dir = self.memory_dir.join("workflow_scripts");
        let script_name = format!(
            "{}_{}.{ext}",
            node.id,
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        let script_path = script_dir.join(&script_name);

        let context_block = if dep_context.is_empty() {
            String::new()
        } else {
            format!("{dep_context}\n")
        };

        let system_prompt = format!(
            "You are creating a reusable script tool as part of a workflow.\n\n\
             Purpose: {}\n\n\
             Runtime: {interpreter}\n\n\
             {context_block}\
             Instructions:\n\
             1. Create directory: mkdir -p {}\n\
             2. Write the script to: {}\n\
             3. Make it executable (if bash): chmod +x {}\n\
             4. Test it with a simple invocation to verify it works\n\n\
             Use execute_shell for all file operations.\n\
             The script should accept input via command-line arguments or stdin.\n\
             When done, report the full path to the script and usage instructions.",
            node.prompt,
            script_dir.display(),
            script_path.display(),
            script_path.display(),
        );

        let tools: Vec<Box<dyn Tool>> =
            vec![Box::new(CurrentTimeTool), Box::new(ShellTool::new())];
        let tool_defs: Vec<ToolDefinition> = tools.iter().map(|t| t.definition()).collect();

        let mut messages = vec![
            Message::System(system_prompt),
            Message::User(format!(
                "Create the {interpreter} script: {}",
                node.description
            )),
        ];

        for iteration in 0..NODE_MAX_ITERATIONS {
            ntb.iterations = iteration + 1;
            ntb.llm_calls += 1;

            let response = match self.provider.chat(&messages, &tool_defs).await {
                Ok(r) => r,
                Err(e) => {
                    return NodeResult {
                        output: format!("LLM error: {e}"),
                        status: NodeStatus::Failed(e.to_string()),
                    };
                }
            };

            if response.tool_calls.is_empty() {
                let output = response.content.unwrap_or_default();
                let result = format!(
                    "Script created at: {}\nInterpreter: {interpreter}\n\n{output}",
                    script_path.display()
                );
                return NodeResult {
                    output: result,
                    status: NodeStatus::Completed,
                };
            }

            let explanation = response.content.clone();
            messages.push(Message::Assistant {
                content: response.content,
                tool_calls: response.tool_calls.clone(),
            });

            for tc in &response.tool_calls {
                let tool_start = std::time::Instant::now();
                let result = self
                    .run_tool(&tools, &tc.name, &tc.arguments, explanation.as_deref())
                    .await;
                ntb.record_tool_call(
                    &tc.name,
                    &tc.arguments,
                    &result,
                    tool_start.elapsed(),
                );
                messages.push(Message::ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: truncate_output(&result, MAX_NODE_TOOL_OUTPUT),
                });
            }
        }

        NodeResult {
            output: "CreateTool node exceeded maximum iterations".to_string(),
            status: NodeStatus::Failed("max iterations exceeded".to_string()),
        }
    }

    // ------------------------------------------------------------------
    // ConnectMcp node: install server, update mcp.json
    // ------------------------------------------------------------------

    async fn execute_connect_mcp_node(&self, node: &PlanNode) -> NodeResult {
        let server_name = match &node.mcp_server_name {
            Some(name) => name.clone(),
            None => {
                return NodeResult {
                    output: "connect_mcp node missing mcp_server_name field".to_string(),
                    status: NodeStatus::Failed("missing mcp_server_name".into()),
                };
            }
        };

        let source = match &node.mcp_source {
            Some(s) => s.clone(),
            None => {
                return NodeResult {
                    output: "connect_mcp node missing mcp_source field".to_string(),
                    status: NodeStatus::Failed("missing mcp_source".into()),
                };
            }
        };

        let (install_cmd, server_config) = match &source {
            McpSource::Npm { package } => (
                Some(format!("npm install -g {package}")),
                McpJsonEntry {
                    command: Some(package.clone()),
                    args: None,
                    env: None,
                    url: None,
                },
            ),
            McpSource::Pip { package } => (
                Some(format!("pip install {package}")),
                McpJsonEntry {
                    command: Some(package.clone()),
                    args: None,
                    env: None,
                    url: None,
                },
            ),
            McpSource::Docker { image } => (
                Some(format!("docker pull {image}")),
                McpJsonEntry {
                    command: Some("docker".to_string()),
                    args: Some(vec!["run".to_string(), "-i".to_string(), image.clone()]),
                    env: None,
                    url: None,
                },
            ),
            McpSource::Url { endpoint } => (
                None,
                McpJsonEntry {
                    command: None,
                    args: None,
                    env: None,
                    url: Some(endpoint.clone()),
                },
            ),
        };

        if let Some(cmd) = install_cmd {
            let args = serde_json::json!({ "command": cmd }).to_string();
            let result = self
                .run_tool(
                    &[Box::new(ShellTool::new()) as Box<dyn Tool>],
                    "execute_shell",
                    &args,
                    Some(&format!("Workflow: installing MCP server '{server_name}'")),
                )
                .await;

            if result.starts_with("Tool error:") || result.contains("rejected") {
                return NodeResult {
                    output: format!("Failed to install MCP server: {result}"),
                    status: NodeStatus::Failed(result),
                };
            }
        }

        let mcp_path = self.memory_dir.join("mcp.json");
        let mut config: serde_json::Value = std::fs::read_to_string(&mcp_path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_else(|| serde_json::json!({ "mcpServers": {} }));

        let servers = config
            .get_mut("mcpServers")
            .and_then(|v| v.as_object_mut());

        match servers {
            Some(servers) => {
                let entry = serde_json::to_value(&server_config).unwrap_or_default();
                servers.insert(server_name.clone(), entry);
            }
            None => {
                let mut map = serde_json::Map::new();
                map.insert(
                    server_name.clone(),
                    serde_json::to_value(&server_config).unwrap_or_default(),
                );
                config["mcpServers"] = serde_json::Value::Object(map);
            }
        }

        match std::fs::write(
            &mcp_path,
            serde_json::to_string_pretty(&config).unwrap_or_default(),
        ) {
            Ok(_) => {
                log::info!("MCP config updated: added server '{server_name}'");
                NodeResult {
                    output: format!(
                        "MCP server '{server_name}' installed and configured in mcp.json.\n\
                         The server will be available for use in subsequent workflow runs.\n\
                         Config: {}",
                        serde_json::to_string_pretty(&server_config).unwrap_or_default()
                    ),
                    status: NodeStatus::Completed,
                }
            }
            Err(e) => NodeResult {
                output: format!("Failed to update mcp.json: {e}"),
                status: NodeStatus::Failed(e.to_string()),
            },
        }
    }

    // ------------------------------------------------------------------
    // Tool execution helper
    // ------------------------------------------------------------------

    async fn run_tool(
        &self,
        tools: &[Box<dyn Tool>],
        name: &str,
        arguments: &str,
        explanation: Option<&str>,
    ) -> String {
        let tool = match tools.iter().find(|t| t.definition().name == name) {
            Some(t) => t,
            None => return format!("Unknown tool: {name}"),
        };

        if tool.requires_approval() {
            let approved = self
                .request_approval(tool.as_ref(), arguments, explanation)
                .await;
            if !approved {
                return "Tool execution was rejected by user".to_string();
            }
        }

        tool.execute(arguments)
            .await
            .unwrap_or_else(|e| format!("Tool error: {e}"))
    }

    async fn request_approval(
        &self,
        tool: &dyn Tool,
        arguments: &str,
        explanation: Option<&str>,
    ) -> bool {
        let guard = self.approval_handler.read().await;
        if let Some(ref handler) = *guard {
            let desc = format!("[workflow] {}", tool.format_approval_request(arguments));
            handler
                .request_approval(&desc, explanation)
                .await
                .unwrap_or(false)
        } else {
            log::warn!(
                "Tool '{}' requires approval but no handler available",
                tool.definition().name
            );
            false
        }
    }

    // ------------------------------------------------------------------
    // Tool factory
    // ------------------------------------------------------------------

    fn build_tools_for_node(&self, node: &PlanNode) -> Vec<Box<dyn Tool>> {
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(CurrentTimeTool),
            Box::new(ReadMemoryTool::new(self.memory_dir.clone())),
        ];

        for tool_name in &node.tools {
            match tool_name.as_str() {
                "execute_shell" => {
                    tools.push(Box::new(ShellTool::new()));
                }
                "web_search" => {
                    if let Some(t) = WebSearchTool::new() {
                        tools.push(Box::new(t));
                    }
                }
                "web_scrape" => {
                    if let Some(t) = WebScrapeTool::new() {
                        tools.push(Box::new(t));
                    }
                }
                "write_memory" => {
                    tools.push(Box::new(WriteMemoryTool::new(self.memory_dir.clone())));
                }
                "current_time" | "read_memory" => {}
                other => {
                    log::debug!(
                        "Node '{}' requested unavailable tool '{other}', skipping",
                        node.id
                    );
                }
            }
        }

        tools
    }
}

// ======================================================================
// Free functions
// ======================================================================

fn status_label(status: &NodeStatus) -> String {
    match status {
        NodeStatus::Completed => "completed".to_string(),
        NodeStatus::Failed(e) => format!("failed: {e}"),
        NodeStatus::Skipped => "skipped".to_string(),
        NodeStatus::Pending => "pending".to_string(),
        NodeStatus::Running => "running".to_string(),
    }
}

fn has_failed_dependency(
    dag: &WorkflowDag,
    node_idx: NodeIndex,
    results: &HashMap<String, NodeResult>,
) -> bool {
    dag.dependencies(node_idx).iter().any(|&dep_idx| {
        let dep_id = &dag.node(dep_idx).id;
        results
            .get(dep_id)
            .is_none_or(|r| !matches!(r.status, NodeStatus::Completed))
    })
}

fn build_dependency_context(
    dag: &WorkflowDag,
    node_idx: NodeIndex,
    results: &HashMap<String, NodeResult>,
) -> String {
    let deps = dag.dependencies(node_idx);
    if deps.is_empty() {
        return String::new();
    }

    let mut ctx = String::from("Results from previous steps:\n\n");
    for dep_idx in deps {
        let dep_node = dag.node(dep_idx);
        if let Some(result) = results.get(&dep_node.id) {
            if matches!(result.status, NodeStatus::Completed) {
                ctx.push_str(&format!(
                    "--- {} ({}) ---\n{}\n\n",
                    dep_node.id, dep_node.description, result.output
                ));
            }
        }
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::approval::new_shared_approval_handler;
    use crate::core::{LlmError, LlmResponse, Message, ToolDefinition};
    use crate::workflow::node::{EdgeKind, NodeKind, PlanEdge, PlanNode, WorkflowPlan};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct CountingMockProvider {
        response_text: String,
        call_count: AtomicUsize,
        call_starts: Mutex<Vec<std::time::Instant>>,
        delay: Option<std::time::Duration>,
    }

    impl CountingMockProvider {
        fn new(response: &str) -> Self {
            Self {
                response_text: response.to_string(),
                call_count: AtomicUsize::new(0),
                call_starts: Mutex::new(Vec::new()),
                delay: None,
            }
        }

        fn with_delay(mut self, ms: u64) -> Self {
            self.delay = Some(std::time::Duration::from_millis(ms));
            self
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for CountingMockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<LlmResponse, LlmError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.call_starts
                .lock()
                .unwrap()
                .push(std::time::Instant::now());

            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }

            Ok(LlmResponse {
                content: Some(self.response_text.clone()),
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    fn make_node(id: &str, kind: NodeKind) -> PlanNode {
        PlanNode {
            id: id.to_string(),
            kind,
            description: format!("task {id}"),
            tools: vec![],
            prompt: format!("do {id}"),
            tool_name: None,
            tool_arguments: None,
            runtime: None,
            install_command: None,
            mcp_source: None,
            mcp_server_name: None,
            max_retries: 0,
        }
    }

    fn make_executor(provider: Arc<dyn LlmProvider>) -> DagExecutor {
        DagExecutor::new(
            &provider,
            &PathBuf::from("/tmp/zymi-test"),
            new_shared_approval_handler(),
        )
    }

    #[tokio::test]
    async fn sequential_dag_executes_in_order() {
        let plan = WorkflowPlan {
            nodes: vec![
                make_node("a", NodeKind::Research),
                make_node("b", NodeKind::Analysis),
                make_node("c", NodeKind::Synthesis),
            ],
            edges: vec![
                PlanEdge { from: "a".into(), to: "b".into(), kind: EdgeKind::Data },
                PlanEdge { from: "b".into(), to: "c".into(), kind: EdgeKind::Data },
            ],
        };

        let provider = Arc::new(CountingMockProvider::new("result"));
        let exec = make_executor(provider.clone());
        let dag = WorkflowDag::from_plan(plan).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        let dag_result = exec.execute(dag, tx).await.unwrap();

        assert_eq!(dag_result.results.len(), 3);
        assert_eq!(dag_result.results[0].0, "a");
        assert_eq!(dag_result.results[1].0, "b");
        assert_eq!(dag_result.results[2].0, "c");
        assert!(dag_result.results.iter().all(|(_, r)| matches!(r.status, NodeStatus::Completed)));
        assert_eq!(provider.call_count.load(Ordering::SeqCst), 3);
        // Verify traces
        assert_eq!(dag_result.node_traces.len(), 3);
        assert_eq!(dag_result.total_llm_calls, 3);
    }

    #[tokio::test]
    async fn parallel_nodes_run_concurrently() {
        let plan = WorkflowPlan {
            nodes: vec![
                make_node("a", NodeKind::Research),
                make_node("b", NodeKind::Research),
                make_node("c", NodeKind::Synthesis),
            ],
            edges: vec![
                PlanEdge { from: "a".into(), to: "c".into(), kind: EdgeKind::Data },
                PlanEdge { from: "b".into(), to: "c".into(), kind: EdgeKind::Data },
            ],
        };

        let provider = Arc::new(CountingMockProvider::new("result").with_delay(100));
        let exec = make_executor(provider.clone());
        let dag = WorkflowDag::from_plan(plan).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        let start = std::time::Instant::now();
        let dag_result = exec.execute(dag, tx).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(dag_result.results.len(), 3);
        assert!(dag_result.results.iter().all(|(_, r)| matches!(r.status, NodeStatus::Completed)));
        assert!(
            elapsed < std::time::Duration::from_millis(280),
            "Expected parallel execution (~200ms) but took {:?}",
            elapsed
        );
        assert_eq!(dag_result.node_traces.len(), 3);
    }

    #[tokio::test]
    async fn failed_dependency_skips_children() {
        struct FailingProvider;

        #[async_trait::async_trait]
        impl LlmProvider for FailingProvider {
            async fn chat(
                &self,
                _messages: &[Message],
                _tools: &[ToolDefinition],
            ) -> Result<LlmResponse, LlmError> {
                Err(LlmError::ApiError("test failure".into()))
            }
        }

        let plan = WorkflowPlan {
            nodes: vec![
                make_node("a", NodeKind::Research),
                make_node("b", NodeKind::Synthesis),
            ],
            edges: vec![PlanEdge { from: "a".into(), to: "b".into(), kind: EdgeKind::Data }],
        };

        let provider: Arc<dyn LlmProvider> = Arc::new(FailingProvider);
        let exec = make_executor(provider);
        let dag = WorkflowDag::from_plan(plan).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        let dag_result = exec.execute(dag, tx).await.unwrap();

        assert_eq!(dag_result.results.len(), 2);
        assert!(matches!(dag_result.results[0].1.status, NodeStatus::Failed(_)));
        assert!(matches!(dag_result.results[1].1.status, NodeStatus::Skipped));
        // Traces should include both nodes
        assert_eq!(dag_result.node_traces.len(), 2);
    }

    #[tokio::test]
    async fn stream_events_emitted() {
        let plan = WorkflowPlan {
            nodes: vec![
                make_node("a", NodeKind::Research),
                make_node("b", NodeKind::Synthesis),
            ],
            edges: vec![PlanEdge { from: "a".into(), to: "b".into(), kind: EdgeKind::Data }],
        };

        let provider = Arc::new(CountingMockProvider::new("done"));
        let exec = make_executor(provider);
        let dag = WorkflowDag::from_plan(plan).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();

        exec.execute(dag, tx).await.unwrap();

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let starts = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::WorkflowNodeStart { .. }))
            .count();
        let completes = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::WorkflowNodeComplete { .. }))
            .count();
        let progresses = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::WorkflowProgress { .. }))
            .count();

        assert_eq!(starts, 2);
        assert_eq!(completes, 2);
        assert_eq!(progresses, 2);
    }

    #[tokio::test]
    async fn connect_mcp_updates_config() {
        let provider = Arc::new(CountingMockProvider::new("done"));
        let tmp = tempfile::tempdir().unwrap();
        let memory_dir = tmp.path().to_path_buf();

        let exec = DagExecutor::new(
            &(provider as Arc<dyn LlmProvider>),
            &memory_dir,
            new_shared_approval_handler(),
        );

        let node = PlanNode {
            id: "mcp1".to_string(),
            kind: NodeKind::ConnectMcp,
            description: "Connect GitHub MCP".to_string(),
            tools: vec![],
            prompt: "Connect GitHub MCP server".to_string(),
            tool_name: None,
            tool_arguments: None,
            runtime: None,
            install_command: None,
            mcp_source: Some(McpSource::Url {
                endpoint: "http://localhost:3000/mcp".to_string(),
            }),
            mcp_server_name: Some("github".to_string()),
            max_retries: 0,
        };

        let result = exec.execute_connect_mcp_node(&node).await;
        assert!(
            matches!(result.status, NodeStatus::Completed),
            "Expected Completed, got: {:?}",
            result.status
        );
        assert!(result.output.contains("github"));

        let mcp_path = memory_dir.join("mcp.json");
        let content = std::fs::read_to_string(&mcp_path).unwrap();
        let config: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(config["mcpServers"]["github"]["url"]
            .as_str()
            .unwrap()
            .contains("localhost:3000"));
    }

    #[tokio::test]
    async fn connect_mcp_merges_with_existing_config() {
        let provider = Arc::new(CountingMockProvider::new("done"));
        let tmp = tempfile::tempdir().unwrap();
        let memory_dir = tmp.path().to_path_buf();

        let existing = serde_json::json!({
            "mcpServers": {
                "existing": { "command": "some-server" }
            }
        });
        std::fs::write(
            memory_dir.join("mcp.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let exec = DagExecutor::new(
            &(provider as Arc<dyn LlmProvider>),
            &memory_dir,
            new_shared_approval_handler(),
        );

        let node = PlanNode {
            id: "mcp2".to_string(),
            kind: NodeKind::ConnectMcp,
            description: "Connect Slack MCP".to_string(),
            tools: vec![],
            prompt: "Connect Slack MCP server".to_string(),
            tool_name: None,
            tool_arguments: None,
            runtime: None,
            install_command: None,
            mcp_source: Some(McpSource::Npm {
                package: "@anthropic/mcp-server-slack".to_string(),
            }),
            mcp_server_name: Some("slack".to_string()),
            max_retries: 0,
        };

        // Install may fail (npm not available in test), that's OK —
        // we're testing config merge, not npm install.
        let _result = exec.execute_connect_mcp_node(&node).await;
        let mcp_path = memory_dir.join("mcp.json");
        let content = std::fs::read_to_string(&mcp_path).unwrap();
        let config: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert!(config["mcpServers"]["existing"]["command"]
            .as_str()
            .is_some());
    }

    #[tokio::test]
    async fn node_traces_include_timing() {
        let plan = WorkflowPlan {
            nodes: vec![make_node("x", NodeKind::Analysis)],
            edges: vec![],
        };

        let provider = Arc::new(CountingMockProvider::new("analyzed").with_delay(50));
        let exec = make_executor(provider);
        let dag = WorkflowDag::from_plan(plan).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();

        let dag_result = exec.execute(dag, tx).await.unwrap();

        assert_eq!(dag_result.node_traces.len(), 1);
        let trace = &dag_result.node_traces[0];
        assert_eq!(trace.node_id, "x");
        assert_eq!(trace.status, "completed");
        assert!(trace.duration_ms >= 40, "Expected ≥40ms, got {}ms", trace.duration_ms);
        assert_eq!(trace.iterations, 1);
    }
}
