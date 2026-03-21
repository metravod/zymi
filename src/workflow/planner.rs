use std::sync::Arc;

use crate::core::{LlmProvider, Message};

use super::assessment::Assessment;
use super::node::WorkflowPlan;
use super::{ToolInfo, WorkflowError};

const PLANNER_PROMPT: &str = "\
You are a workflow planner. Given a user's task and its complexity assessment, \
create an execution plan as a directed acyclic graph (DAG).

## Available tools

{tools}

## Node kinds

- \"research\": Gather information (web_search, web_scrape, read_memory, etc.)
- \"code_gen\": Generate or modify code, scripts, configurations
- \"analysis\": Reason over data, compare options, draw conclusions
- \"tool_call\": Direct call to a specific tool (set tool_name and tool_arguments)
- \"create_tool\": Write a custom script when no existing tool fits the task. \
Set \"runtime\" field to \"python\", \"bash\", or \"node\". \
The executor will generate, save, and test the script automatically. \
Subsequent nodes can use its output (the script path) via execute_shell.
- \"connect_mcp\": Connect an MCP server not yet available. \
Set \"mcp_server_name\" and \"mcp_source\" (with type: \"npm\"/\"pip\"/\"docker\"/\"url\" and the corresponding field). \
The server will be installed and registered in mcp.json.
- \"install_dep\": Install a CLI tool or library (set install_command)
- \"synthesis\": Combine results from other nodes into the final user-facing answer

## Response format

Respond with ONLY valid JSON, no markdown fences:
{
  \"nodes\": [
    {
      \"id\": \"unique_id\",
      \"kind\": \"<node kind>\",
      \"description\": \"Brief description of what this node does\",
      \"tools\": [\"tool1\", \"tool2\"],
      \"prompt\": \"Detailed instruction for the agent executing this node.\"
    }
  ],
  \"edges\": [
    {\"from\": \"source_id\", \"to\": \"target_id\", \"kind\": \"data\"}
  ]
}

## Rules

- The graph MUST be acyclic (no circular dependencies)
- Independent nodes will execute IN PARALLEL — maximize parallelism where possible
- The final node should typically be \"synthesis\" to combine results
- Keep the graph minimal — don't over-decompose simple subtasks
- Each node prompt must be self-contained and detailed enough for an isolated agent
- For \"tool_call\" nodes: include \"tool_name\" and \"tool_arguments\" fields
- For \"create_tool\" nodes: include \"runtime\" field (\"python\", \"bash\", or \"node\")
- For \"install_dep\" nodes: include \"install_command\" field
- For \"connect_mcp\" nodes: include \"mcp_server_name\" and \"mcp_source\" fields
- Optionally set \"max_retries\" (0-3, default: 1) for nodes that might need retry on failure
- Only reference tools from the available list unless creating new ones
- If a task requires capabilities not covered by existing tools, prefer \"create_tool\" \
(for data processing, analysis, file conversion) or \"connect_mcp\" (for external service integrations)";

/// Format the tool catalog for the planner prompt.
fn format_tool_catalog(tools: &[ToolInfo]) -> String {
    if tools.is_empty() {
        return "execute_shell, web_search, web_scrape, read_memory, write_memory".to_string();
    }

    let mut catalog = String::new();
    for tool in tools {
        // Truncate very long descriptions to keep the prompt manageable
        let desc = if tool.description.len() > 200 {
            let end = tool.description.floor_char_boundary(200);
            format!("{}...", &tool.description[..end])
        } else {
            tool.description.clone()
        };
        catalog.push_str(&format!("- **{}**: {}\n", tool.name, desc));
    }
    catalog
}

pub async fn create_plan(
    provider: &Arc<dyn LlmProvider>,
    user_message: &str,
    assessment: &Assessment,
    available_tools: &[ToolInfo],
) -> Result<WorkflowPlan, WorkflowError> {
    let tools_section = format_tool_catalog(available_tools);
    let system_prompt = PLANNER_PROMPT.replace("{tools}", &tools_section);

    let user_prompt = format!(
        "Task: {user_message}\n\n\
         Complexity assessment (score {}/10): {}\n\
         Suggested approach: {}",
        assessment.score, assessment.reasoning, assessment.suggested_approach
    );

    let messages = vec![
        Message::System(system_prompt),
        Message::User(user_prompt),
    ];

    let response = provider.chat(&messages, &[]).await?;
    let content = response
        .content
        .ok_or_else(|| WorkflowError::PlanningFailed("empty planner response".into()))?;

    let json_str = super::assessment::extract_json(&content);

    let plan: WorkflowPlan = serde_json::from_str(json_str).map_err(|e| {
        WorkflowError::PlanningFailed(format!("failed to parse plan: {e}\nraw: {content}"))
    })?;

    if plan.nodes.is_empty() {
        return Err(WorkflowError::PlanningFailed(
            "plan has no nodes".to_string(),
        ));
    }

    log::info!(
        "Workflow plan: {} nodes, {} edges",
        plan.nodes.len(),
        plan.edges.len()
    );

    Ok(plan)
}
