pub mod assessment;
pub mod dag;
pub mod executor;
pub mod node;
pub mod planner;
pub mod trace;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;
use tokio::sync::mpsc;

use crate::core::approval::SharedApprovalHandler;
use crate::core::{LlmProvider, Message, StreamEvent};

pub use self::node::{NodeResult, NodeStatus};

use self::dag::WorkflowDag;
use self::trace::{AssessmentTrace, DagTrace, PhaseTrace, PlanningTrace, TraceBuilder};

/// Complexity score at or below which the standard agent loop is used.
const SIMPLE_THRESHOLD: u8 = 5;

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("simple task (score {score}/10), use standard agent")]
    SimpleTask { score: u8 },
    #[error("planning failed: {0}")]
    PlanningFailed(String),
    #[error("invalid DAG: {0}")]
    InvalidDag(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error("LLM error: {0}")]
    LlmError(#[from] crate::core::LlmError),
}

/// Tool metadata passed to the workflow planner so it knows what's available.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

/// Result of a workflow execution, including side-effects metadata.
pub struct WorkflowResult {
    pub response: String,
    /// MCP server names that were connected during this workflow (ConnectMcp nodes).
    pub new_mcp_servers: Vec<String>,
}

const SYNTHESIS_PROMPT: &str = "\
You are synthesizing results from a multi-step workflow into a final response for the user.

You will receive the original user request and the outputs of each workflow step.
Combine them into a clear, well-structured answer. Do NOT mention the workflow, \
steps, or nodes — respond as if you did all the work yourself.

Be concise but thorough. Use the same language the user wrote in.";

pub struct WorkflowEngine {
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    available_tools: Vec<ToolInfo>,
    approval_handler: SharedApprovalHandler,
}

impl WorkflowEngine {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        memory_dir: PathBuf,
        available_tools: Vec<ToolInfo>,
        approval_handler: SharedApprovalHandler,
    ) -> Self {
        Self {
            provider,
            memory_dir,
            available_tools,
            approval_handler,
        }
    }

    /// Run the full workflow pipeline: assess → plan → DAG → execute → synthesize.
    ///
    /// Returns `Err(WorkflowError::SimpleTask)` if the request is too simple
    /// and should be handled by the standard agent loop.
    pub async fn process(
        &self,
        user_message: &str,
        event_tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<WorkflowResult, WorkflowError> {
        let mut tb = TraceBuilder::new(user_message);

        // 1. Assess complexity (try heuristic first, fallback to LLM)
        let assess_start = Instant::now();
        let assessment = match assessment::quick_assess(user_message) {
            Some(a) => a,
            None => {
                let a = assessment::assess_complexity(&self.provider, user_message).await?;
                tb.total_llm_calls += 1;
                a
            }
        };

        log::info!(
            "Workflow assessment: score={}, reasoning={}",
            assessment.score,
            assessment.reasoning
        );

        if assessment.score <= SIMPLE_THRESHOLD {
            return Err(WorkflowError::SimpleTask {
                score: assessment.score,
            });
        }

        tb.assessment = Some(AssessmentTrace {
            score: assessment.score,
            reasoning: assessment.reasoning.clone(),
            duration_ms: assess_start.elapsed().as_millis() as u64,
        });

        let _ = event_tx.send(StreamEvent::WorkflowAssessment {
            score: assessment.score,
            reasoning: assessment.reasoning.clone(),
        });

        // 2. Plan
        let plan_start = Instant::now();
        let plan = planner::create_plan(
            &self.provider,
            user_message,
            &assessment,
            &self.available_tools,
        )
        .await?;
        tb.total_llm_calls += 1;

        let raw_plan_json =
            serde_json::to_string_pretty(&plan).unwrap_or_else(|_| "{}".to_string());
        tb.planning = Some(PlanningTrace {
            node_count: plan.nodes.len(),
            edge_count: plan.edges.len(),
            raw_plan_json,
            duration_ms: plan_start.elapsed().as_millis() as u64,
        });

        let _ = event_tx.send(StreamEvent::WorkflowPlanReady {
            node_count: plan.nodes.len(),
            edge_count: plan.edges.len(),
        });

        // 3. Build DAG
        let dag = WorkflowDag::from_plan(plan)?;

        // Record DAG structure in trace
        if let Ok(levels) = dag.execution_levels() {
            tb.dag_structure = Some(DagTrace {
                total_nodes: dag.node_count(),
                levels: levels
                    .iter()
                    .map(|level| level.iter().map(|&idx| dag.node(idx).id.clone()).collect())
                    .collect(),
            });
        }

        // 3.5. Approve plan
        if !self.request_plan_approval(&dag).await {
            return Err(WorkflowError::PlanningFailed(
                "workflow plan rejected by user".into(),
            ));
        }

        // 4. Execute
        let exec = executor::DagExecutor::new(
            &self.provider,
            &self.memory_dir,
            self.approval_handler.clone(),
        );
        let dag_result = exec.execute(dag, event_tx.clone()).await?;

        let new_mcp_servers = dag_result.new_mcp_servers.clone();

        tb.nodes = dag_result.node_traces;
        tb.total_llm_calls += dag_result.total_llm_calls;
        tb.total_tool_calls += dag_result.total_tool_calls;

        // 5. Synthesize (skip for single completed node)
        let completed_nodes: Vec<_> = dag_result
            .results
            .iter()
            .filter(|(_, r)| matches!(r.status, NodeStatus::Completed))
            .collect();

        let response = if completed_nodes.len() == 1 {
            log::info!("Single completed node — skipping synthesis LLM call");
            completed_nodes[0].1.output.clone()
        } else {
            let synth_start = Instant::now();
            let r = self.synthesize(user_message, &dag_result.results).await?;
            tb.total_llm_calls += 1;
            tb.synthesis = Some(PhaseTrace {
                duration_ms: synth_start.elapsed().as_millis() as u64,
            });
            r
        };

        // 6. Build & save trace
        let trace = tb.finish("success");
        let summary = trace.summary();
        let trace_path = trace.save(&self.memory_dir);

        log::info!("Workflow completed:\n{summary}");

        let _ = event_tx.send(StreamEvent::WorkflowTraceReady {
            summary,
            trace_path: trace_path.map(|p| p.display().to_string()),
        });

        Ok(WorkflowResult {
            response,
            new_mcp_servers,
        })
    }

    async fn request_plan_approval(&self, dag: &WorkflowDag) -> bool {
        let levels = match dag.execution_levels() {
            Ok(l) => l,
            Err(_) => return true,
        };

        let mut plan_desc = String::from("Workflow plan:\n");
        for (i, level) in levels.iter().enumerate() {
            let parallel = if level.len() > 1 { " [parallel]" } else { "" };
            for &idx in level {
                let node = dag.node(idx);
                plan_desc.push_str(&format!(
                    "  Level {i}{parallel}: [{}] {}\n",
                    node.id, node.description
                ));
            }
        }

        let guard = self.approval_handler.read().await;
        match &*guard {
            Some(handler) => handler
                .request_approval(&plan_desc, Some("Approve workflow plan?"))
                .await
                .unwrap_or(false),
            None => true,
        }
    }

    /// Combine all node results into the final user-facing response.
    async fn synthesize(
        &self,
        user_message: &str,
        results: &[(String, NodeResult)],
    ) -> Result<String, WorkflowError> {
        let mut steps = String::new();
        for (id, result) in results {
            if matches!(result.status, NodeStatus::Completed) {
                steps.push_str(&format!("## Step: {id}\n{}\n\n", result.output));
            } else {
                steps.push_str(&format!(
                    "## Step: {id} [FAILED]\n{}\n\n",
                    result.output
                ));
            }
        }

        let messages = vec![
            Message::System(SYNTHESIS_PROMPT.to_string()),
            Message::User(format!(
                "Original user request:\n{user_message}\n\n\
                 Workflow results:\n{steps}"
            )),
        ];

        let response = self.provider.chat(&messages, &[]).await?;
        response
            .content
            .ok_or_else(|| WorkflowError::ExecutionFailed("empty synthesis response".into()))
    }
}
