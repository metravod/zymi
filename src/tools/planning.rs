use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::core::agent::Agent;
use crate::core::{LlmProvider, ToolDefinition};
use crate::storage::in_memory::InMemoryStorage;
use crate::tools::current_time::CurrentTimeTool;
use crate::tools::memory::ReadMemoryTool;
use crate::tools::web_search::WebSearchTool;
use crate::tools::Tool;

#[derive(Deserialize)]
struct Approach {
    name: String,
    description: String,
    pros: Vec<String>,
    cons: Vec<String>,
    #[serde(default)]
    simulation_task: Option<String>,
}

#[derive(Deserialize)]
struct PlanningArgs {
    task_analysis: String,
    approaches: Vec<Approach>,
    #[serde(default)]
    selected_approach: Option<String>,
    #[serde(default)]
    execution_steps: Option<Vec<String>>,
    #[serde(default)]
    delegation_needed: bool,
    #[serde(default)]
    delegation_plan: Option<String>,
}

pub struct PlanningTool {
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
}

impl PlanningTool {
    pub fn new(provider: Arc<dyn LlmProvider>, memory_dir: PathBuf) -> Self {
        Self {
            provider,
            memory_dir,
        }
    }

    async fn run_simulation(&self, approach_name: &str, task: &str) -> String {
        let system_prompt = format!(
            "You are a feasibility assessor. Your job is to assess whether the following approach is feasible.\n\
            Approach: \"{approach_name}\"\n\n\
            You have access to read-only tools: current time, memory files, and web search.\n\
            Investigate the task, then give a short verdict.\n\n\
            End your response with exactly one of:\n\
            VERDICT: FEASIBLE\n\
            VERDICT: INFEASIBLE\n\
            VERDICT: UNCERTAIN\n\n\
            Followed by a one-sentence justification."
        );

        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(CurrentTimeTool),
            Box::new(ReadMemoryTool::new(self.memory_dir.clone())),
        ];

        if let Some(tool) = WebSearchTool::new() {
            tools.push(Box::new(tool));
        }

        let storage = Arc::new(InMemoryStorage::new());
        let agent = Agent::new(
            self.provider.clone(),
            tools,
            Some(system_prompt),
            storage,
        )
        .with_max_iterations(3);

        let conversation_id = format!("simulation-{}", uuid::Uuid::new_v4());

        match agent.process(&conversation_id, task, None).await {
            Ok(response) => response,
            Err(e) => format!("Simulation error: {e}"),
        }
    }
}

#[async_trait]
impl Tool for PlanningTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "think".to_string(),
            description: "Plan before acting. MUST be called for any non-trivial task.\n\
                - Analyze the task and propose 2+ approaches with pros/cons.\n\
                - Optionally add simulation_task per approach — sub-agents will probe feasibility \
                (read-only: memory, web search, time) and return FEASIBLE/INFEASIBLE/UNCERTAIN.\n\
                - After analysis (and simulations if any), set selected_approach and execution_steps."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_analysis": {
                        "type": "string",
                        "description": "What is the task? What are the constraints and requirements?"
                    },
                    "approaches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "Short name for the approach" },
                                "description": { "type": "string", "description": "How this approach works" },
                                "pros": { "type": "array", "items": { "type": "string" }, "description": "Advantages" },
                                "cons": { "type": "array", "items": { "type": "string" }, "description": "Disadvantages" },
                                "simulation_task": {
                                    "type": "string",
                                    "description": "Optional task for a sub-agent to probe feasibility of this approach. E.g. 'Check if the X API supports Y by searching the web' or 'Read memory files to see if Z is configured'."
                                }
                            },
                            "required": ["name", "description", "pros", "cons"]
                        },
                        "minItems": 2,
                        "description": "At least 2 approaches to consider"
                    },
                    "selected_approach": {
                        "type": "string",
                        "description": "Which approach you chose and why (can be omitted if you want to decide after seeing simulation results)"
                    },
                    "execution_steps": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Concrete steps to execute the chosen approach (can be omitted if selected_approach is omitted)"
                    },
                    "delegation_needed": {
                        "type": "boolean",
                        "description": "Whether any steps should be delegated to sub-agents"
                    },
                    "delegation_plan": {
                        "type": "string",
                        "description": "If delegation_needed=true, which sub-agents to use and what tasks to give them"
                    }
                },
                "required": ["task_analysis", "approaches"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: PlanningArgs =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        if args.approaches.len() < 2 {
            return Err("At least 2 approaches are required.".to_string());
        }

        let mut plan = String::new();

        plan.push_str("## Task Analysis\n");
        plan.push_str(&args.task_analysis);
        plan.push_str("\n\n");

        plan.push_str("## Approaches Considered\n");
        for (i, approach) in args.approaches.iter().enumerate() {
            plan.push_str(&format!("### {}. {}\n", i + 1, approach.name));
            plan.push_str(&approach.description);
            plan.push('\n');
            plan.push_str("**Pros:**\n");
            for pro in &approach.pros {
                plan.push_str(&format!("  + {pro}\n"));
            }
            plan.push_str("**Cons:**\n");
            for con in &approach.cons {
                plan.push_str(&format!("  - {con}\n"));
            }
            plan.push('\n');
        }

        // Collect and run simulations in parallel
        let simulation_futures: Vec<_> = args
            .approaches
            .iter()
            .filter_map(|a| {
                a.simulation_task.as_ref().map(|task| {
                    let name = a.name.clone();
                    let task = task.clone();
                    async move {
                        let result = self.run_simulation(&name, &task).await;
                        (name, result)
                    }
                })
            })
            .collect();

        if !simulation_futures.is_empty() {
            let results = futures::future::join_all(simulation_futures).await;

            plan.push_str("## Simulation Results\n");
            for (name, result) in &results {
                plan.push_str(&format!("### {name}\n"));
                plan.push_str(result);
                plan.push_str("\n\n");
            }
        }

        if let Some(ref selected) = args.selected_approach {
            plan.push_str("## Selected Approach\n");
            plan.push_str(selected);
            plan.push_str("\n\n");
        }

        if let Some(ref steps) = args.execution_steps {
            plan.push_str("## Execution Steps\n");
            for (i, step) in steps.iter().enumerate() {
                plan.push_str(&format!("{}. {step}\n", i + 1));
            }
        }

        if args.delegation_needed {
            plan.push_str("\n## Delegation Plan\n");
            if let Some(ref dp) = args.delegation_plan {
                plan.push_str(dp);
            } else {
                plan.push_str("(delegation needed but no plan specified)");
            }
            plan.push('\n');
        }

        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{LlmError, LlmResponse, Message, ToolDefinition as CoreToolDef};
    use tokio::sync::mpsc;

    struct MockProvider {
        response: String,
    }

    impl MockProvider {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[CoreToolDef],
        ) -> Result<LlmResponse, LlmError> {
            Ok(LlmResponse {
                content: Some(self.response.clone()),
                tool_calls: vec![],
                usage: None,
            })
        }

        async fn chat_stream(
            &self,
            messages: &[Message],
            tools: &[CoreToolDef],
            _tx: mpsc::UnboundedSender<crate::core::StreamEvent>,
        ) -> Result<LlmResponse, LlmError> {
            self.chat(messages, tools).await
        }
    }

    fn make_tool(response: &str) -> PlanningTool {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(response));
        let memory_dir = std::env::temp_dir().join("zymi_test_planning");
        std::fs::create_dir_all(&memory_dir).ok();
        PlanningTool::new(provider, memory_dir)
    }

    fn valid_args(approaches: usize, delegation: bool) -> String {
        let mut approaches_vec = Vec::new();
        for i in 0..approaches {
            approaches_vec.push(serde_json::json!({
                "name": format!("Approach {}", i + 1),
                "description": format!("Description {}", i + 1),
                "pros": ["fast"],
                "cons": ["complex"],
            }));
        }
        let args = serde_json::json!({
            "task_analysis": "Test task",
            "approaches": approaches_vec,
            "selected_approach": "Approach 1 because it's fast",
            "execution_steps": ["Step 1", "Step 2"],
            "delegation_needed": delegation,
            "delegation_plan": if delegation { Some("Delegate step 2 to web-searcher") } else { None },
        });
        args.to_string()
    }

    fn args_with_simulation() -> String {
        serde_json::json!({
            "task_analysis": "Test task with simulation",
            "approaches": [
                {
                    "name": "Approach A",
                    "description": "First approach",
                    "pros": ["simple"],
                    "cons": ["slow"],
                    "simulation_task": "Check if approach A is feasible"
                },
                {
                    "name": "Approach B",
                    "description": "Second approach",
                    "pros": ["fast"],
                    "cons": ["complex"]
                }
            ]
        })
        .to_string()
    }

    fn args_without_selected() -> String {
        serde_json::json!({
            "task_analysis": "Test task without selection",
            "approaches": [
                {
                    "name": "Approach X",
                    "description": "First",
                    "pros": ["a"],
                    "cons": ["b"]
                },
                {
                    "name": "Approach Y",
                    "description": "Second",
                    "pros": ["c"],
                    "cons": ["d"]
                }
            ]
        })
        .to_string()
    }

    #[tokio::test]
    async fn execute_with_two_approaches() {
        let tool = make_tool("");
        let result = tool.execute(&valid_args(2, false)).await.unwrap();
        assert!(result.contains("## Task Analysis"));
        assert!(result.contains("Approach 1"));
        assert!(result.contains("Approach 2"));
        assert!(result.contains("## Selected Approach"));
        assert!(result.contains("## Execution Steps"));
        assert!(!result.contains("## Delegation Plan"));
    }

    #[tokio::test]
    async fn execute_with_one_approach_fails() {
        let tool = make_tool("");
        let result = tool.execute(&valid_args(1, false)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("At least 2"));
    }

    #[tokio::test]
    async fn execute_with_delegation() {
        let tool = make_tool("");
        let result = tool.execute(&valid_args(2, true)).await.unwrap();
        assert!(result.contains("## Delegation Plan"));
        assert!(result.contains("web-searcher"));
    }

    #[tokio::test]
    async fn execute_with_invalid_json() {
        let tool = make_tool("");
        let result = tool.execute("not json").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid arguments"));
    }

    #[tokio::test]
    async fn execute_with_simulation() {
        let tool = make_tool("VERDICT: FEASIBLE\nThis approach works fine.");
        let result = tool.execute(&args_with_simulation()).await.unwrap();
        assert!(result.contains("## Simulation Results"));
        assert!(result.contains("Approach A"));
        assert!(result.contains("FEASIBLE"));
        // Approach B has no simulation_task, should not appear in simulation results
        assert!(!result.contains("### Approach B"));
    }

    #[tokio::test]
    async fn execute_without_selected_approach() {
        let tool = make_tool("");
        let result = tool.execute(&args_without_selected()).await.unwrap();
        assert!(result.contains("## Task Analysis"));
        assert!(result.contains("Approach X"));
        assert!(result.contains("Approach Y"));
        // Should not contain selected/steps sections
        assert!(!result.contains("## Selected Approach"));
        assert!(!result.contains("## Execution Steps"));
    }
}
