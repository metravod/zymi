use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::core::{LlmProvider, Message, ToolDefinition};
use crate::eval::{extract_json_from_response, EvalSuite};
use crate::tools::Tool;

pub struct GenerateEvalsTool {
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
}

impl GenerateEvalsTool {
    pub fn new(provider: Arc<dyn LlmProvider>, memory_dir: PathBuf) -> Self {
        Self {
            provider,
            memory_dir,
        }
    }

    fn list_available_agents(&self) -> Vec<String> {
        let subagents_dir = self.memory_dir.join("subagents");
        let entries = match std::fs::read_dir(&subagents_dir) {
            Ok(entries) => entries,
            Err(_) => return vec![],
        };

        let mut agents: Vec<String> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    Some(name.trim_end_matches(".md").to_string())
                } else {
                    None
                }
            })
            .collect();

        agents.sort();
        agents
    }
}

fn validate_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Agent name is required.".to_string());
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("Invalid agent name: path traversal is not allowed.".to_string());
    }
    Ok(())
}

#[async_trait]
impl Tool for GenerateEvalsTool {
    fn definition(&self) -> ToolDefinition {
        let agents = self.list_available_agents();
        let agents_list = if agents.is_empty() {
            "No sub-agents available.".to_string()
        } else {
            format!("Available agents: {}", agents.join(", "))
        };

        ToolDefinition {
            name: "generate_evals".to_string(),
            description: format!(
                "Generate evaluation test cases for a sub-agent based on its system prompt. \
                Creates targeted eval cases that test critical behaviors. \
                Saves results to memory/evals/<agent_name>.json. {}",
                agents_list
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_name": {
                        "type": "string",
                        "description": "Name of the sub-agent to generate evals for (without .md extension)"
                    }
                },
                "required": ["agent_name"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let agent_name = args
            .get("agent_name")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: agent_name")?
            .trim();

        validate_agent_name(agent_name)?;

        let prompt_path = self
            .memory_dir
            .join("subagents")
            .join(format!("{agent_name}.md"));

        let system_prompt = tokio::fs::read_to_string(&prompt_path)
            .await
            .map_err(|e| format!("Cannot read sub-agent '{agent_name}': {e}"))?;

        let generation_prompt = format!(
            r#"You are an eval generator. Given a sub-agent's system prompt, generate a set of evaluation test cases.

## Sub-agent system prompt
{system_prompt}

## Instructions
Generate 3-8 eval cases. Each must test a UNIQUE critical behavior — no overlap.

Focus on:
1. Core task — does the agent perform its primary function?
2. Format/instructions — does the agent follow its output format and rules?
3. Edge cases — unusual inputs, empty input, boundary conditions
4. Quality — is the output high quality and well-structured?

Each eval case needs:
- `id`: short kebab-case identifier (e.g., "cr-security-01")
- `description`: one-line description of what's being tested
- `input`: the exact user message to send to the agent
- `expectations`: what to check in the response

### Available expectations fields:
- `output_contains` (string[]): keywords that MUST appear (case-insensitive, AND logic)
- `output_not_contains` (string[]): keywords that must NOT appear
- `output_any_of` (string[][]): groups of keywords — at least ONE group must fully match
- `output_contains_any_of` (string[]): at least ONE keyword must appear (OR logic)
- `output_matches_regex` (string[]): regex patterns that must match the output
- `tool_calls` (object[]): tools expected to be called, each with `name` field
- `no_tool_calls` (bool): true if agent should NOT use any tools
- `max_tool_calls` (number): maximum number of tool calls allowed
- `min_tool_calls` (number): minimum number of tool calls required
- `mock_responses` (object): custom mock responses per tool name, e.g., {{"web_search": "Result: ..."}}
- `timeout_seconds` (number): timeout per eval case (default: 120)
- `llm_judge` (object|null): LLM-based multi-dimensional scoring
  - `enabled` (bool, default true)
  - `criteria` (string): additional evaluation context for the judge
  - `dimensions` (string[]|null): scoring dimensions (default: correctness, groundedness, relevance, conciseness, tool_usage)
  - `min_score` (number|null): minimum average score to pass (default: 3.0)

## Output format
Respond with ONLY valid JSON, no markdown fences, no explanation:
{{
  "subagent": "{agent_name}",
  "evals": [...]
}}

Tips:
- Keep inputs concise and realistic
- Make expectations specific enough to catch regressions but not so strict they create false failures
- Use llm_judge for nuanced quality assessment where keyword matching isn't sufficient
- Use mock_responses when you want to test how the agent processes tool results"#
        );

        log::info!("Generating evals for sub-agent '{agent_name}'");

        let messages = vec![Message::User(generation_prompt)];

        let response = self
            .provider
            .chat(&messages, &[])
            .await
            .map_err(|e| format!("LLM error during eval generation: {e}"))?;

        let content = response
            .content
            .ok_or("Empty response from LLM during eval generation")?;

        let json_value = extract_json_from_response(&content)?;

        // Validate by deserializing
        let suite: EvalSuite = serde_json::from_value(json_value)
            .map_err(|e| format!("Generated JSON is not a valid EvalSuite: {e}"))?;

        if suite.evals.is_empty() {
            return Err("Generated eval suite has no test cases".to_string());
        }

        // Create evals directory if needed
        let evals_dir = self.memory_dir.join("evals");
        tokio::fs::create_dir_all(&evals_dir)
            .await
            .map_err(|e| format!("Cannot create evals directory: {e}"))?;

        // Write the eval file
        let eval_path = evals_dir.join(format!("{agent_name}.json"));
        let json_str = serde_json::to_string_pretty(&suite)
            .map_err(|e| format!("Cannot serialize eval suite: {e}"))?;

        tokio::fs::write(&eval_path, &json_str)
            .await
            .map_err(|e| format!("Cannot write eval file: {e}"))?;

        log::info!(
            "Generated {} evals for '{}', saved to {}",
            suite.evals.len(),
            agent_name,
            eval_path.display()
        );

        Ok(format!(
            "Generated {} eval cases for '{}':\n{}",
            suite.evals.len(),
            agent_name,
            suite
                .evals
                .iter()
                .map(|e| format!("  - {}: {}", e.id, e.description))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}
