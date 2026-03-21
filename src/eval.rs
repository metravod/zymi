use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::core::agent::Agent;
use crate::core::{LlmProvider, Message, ToolCallInfo, ToolDefinition};
use crate::storage::{ConversationStorage, StorageError};
use crate::tools::current_time::CurrentTimeTool;
use crate::tools::memory::ReadMemoryTool;
use crate::tools::Tool;

// ─── Data structures ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSuite {
    pub subagent: String,
    pub evals: Vec<EvalCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCase {
    pub id: String,
    pub description: String,
    pub input: String,
    pub expectations: Expectations,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expectations {
    #[serde(default)]
    pub output_contains: Vec<String>,
    #[serde(default)]
    pub output_not_contains: Vec<String>,
    #[serde(default)]
    pub output_any_of: Vec<Vec<String>>,
    #[serde(default)]
    pub output_contains_any_of: Vec<String>,
    #[serde(default)]
    pub output_matches_regex: Vec<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallExpectation>,
    #[serde(default)]
    pub no_tool_calls: bool,
    #[serde(default)]
    pub max_tool_calls: Option<usize>,
    #[serde(default)]
    pub min_tool_calls: Option<usize>,
    #[serde(default)]
    pub llm_judge: Option<LlmJudge>,
    #[serde(default)]
    pub mock_responses: Option<HashMap<String, String>>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallExpectation {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmJudge {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub criteria: String,
    #[serde(default)]
    pub dimensions: Option<Vec<String>>,
    #[serde(default)]
    pub min_score: Option<f32>,
}

fn default_true() -> bool {
    true
}

// ─── Default scoring dimensions ─────────────────────────────────────

pub const DEFAULT_DIMENSIONS: &[&str] = &[
    "correctness",
    "groundedness",
    "relevance",
    "conciseness",
    "tool_usage",
];

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const DEFAULT_MIN_SCORE: f32 = 3.0;

// ─── Result structures ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct DimensionScore {
    pub name: String,
    pub score: u8,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalResult {
    pub eval_id: String,
    pub description: String,
    pub passed: bool,
    pub checks: Vec<CheckResult>,
    pub scores: Vec<DimensionScore>,
    pub average_score: Option<f32>,
    pub output: String,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub subagent: String,
    pub results: Vec<EvalResult>,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}

// ─── RecordingStorage ───────────────────────────────────────────────

pub struct RecordingStorage {
    inner: crate::storage::in_memory::InMemoryStorage,
    recorded_tool_calls: Mutex<Vec<ToolCallInfo>>,
}

impl RecordingStorage {
    pub fn new() -> Self {
        Self {
            inner: crate::storage::in_memory::InMemoryStorage::new(),
            recorded_tool_calls: Mutex::new(Vec::new()),
        }
    }

    pub async fn get_recorded_tool_calls(&self) -> Vec<ToolCallInfo> {
        self.recorded_tool_calls.lock().await.clone()
    }
}

#[async_trait]
impl ConversationStorage for RecordingStorage {
    async fn get_history(&self, conversation_id: &str) -> Result<Vec<Message>, StorageError> {
        self.inner.get_history(conversation_id).await
    }

    async fn add_message(
        &self,
        conversation_id: &str,
        message: &Message,
    ) -> Result<(), StorageError> {
        if let Message::Assistant { tool_calls, .. } = message {
            if !tool_calls.is_empty() {
                let mut recorded = self.recorded_tool_calls.lock().await;
                recorded.extend(tool_calls.iter().cloned());
            }
        }
        self.inner.add_message(conversation_id, message).await
    }

    async fn clear(&self, conversation_id: &str) -> Result<(), StorageError> {
        self.inner.clear(conversation_id).await
    }
}

// ─── MockTool ───────────────────────────────────────────────────────

pub struct MockTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    response: String,
}

impl MockTool {
    pub fn new(name: &str, description: &str, parameters: serde_json::Value, response: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            response: response.to_string(),
        }
    }
}

#[async_trait]
impl Tool for MockTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(&self, _arguments: &str) -> Result<String, String> {
        Ok(self.response.clone())
    }
}

// ─── build_eval_tools ───────────────────────────────────────────────

pub fn build_eval_tools(
    memory_dir: &Path,
    mock_responses: Option<&HashMap<String, String>>,
) -> Vec<Box<dyn Tool>> {
    let get_mock = |name: &str, default: &str| -> String {
        mock_responses
            .and_then(|m| m.get(name))
            .cloned()
            .unwrap_or_else(|| default.to_string())
    };

    vec![
        Box::new(CurrentTimeTool),
        Box::new(ReadMemoryTool::new(memory_dir.to_path_buf())),
        Box::new(MockTool::new(
            "write_memory",
            "Write to agent's long-term memory (mock — no-op in eval mode).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "filename": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["filename", "content"]
            }),
            &get_mock(
                "write_memory",
                "[eval mode] write_memory is mocked — no changes were saved.",
            ),
        )),
        Box::new(MockTool::new(
            "web_search",
            "Search the web (mock — returns placeholder in eval mode).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
            &get_mock(
                "web_search",
                "[eval mode] web_search is mocked — no results available.",
            ),
        )),
        Box::new(MockTool::new(
            "web_scrape",
            "Scrape a web page (mock — returns placeholder in eval mode).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" }
                },
                "required": ["url"]
            }),
            &get_mock(
                "web_scrape",
                "[eval mode] web_scrape is mocked — no content available.",
            ),
        )),
    ]
}

// ─── Rule-based checkers ────────────────────────────────────────────

pub fn check_output_contains(output: &str, expected: &[String]) -> Vec<CheckResult> {
    let output_lower = output.to_lowercase();
    expected
        .iter()
        .map(|keyword| {
            let found = output_lower.contains(&keyword.to_lowercase());
            CheckResult {
                name: format!("output_contains(\"{}\")", keyword),
                passed: found,
                detail: if found {
                    format!("Found \"{}\" in output", keyword)
                } else {
                    format!("\"{}\" NOT found in output", keyword)
                },
            }
        })
        .collect()
}

pub fn check_output_not_contains(output: &str, forbidden: &[String]) -> Vec<CheckResult> {
    let output_lower = output.to_lowercase();
    forbidden
        .iter()
        .map(|keyword| {
            let found = output_lower.contains(&keyword.to_lowercase());
            CheckResult {
                name: format!("output_not_contains(\"{}\")", keyword),
                passed: !found,
                detail: if found {
                    format!("Forbidden \"{}\" FOUND in output", keyword)
                } else {
                    format!("\"{}\" correctly absent from output", keyword)
                },
            }
        })
        .collect()
}

pub fn check_output_any_of(output: &str, groups: &[Vec<String>]) -> CheckResult {
    let output_lower = output.to_lowercase();
    let matched_group = groups.iter().position(|group| {
        group
            .iter()
            .all(|kw| output_lower.contains(&kw.to_lowercase()))
    });

    CheckResult {
        name: "output_any_of".to_string(),
        passed: matched_group.is_some(),
        detail: if let Some(idx) = matched_group {
            format!("Keyword group {} fully matched", idx + 1)
        } else {
            format!("None of {} keyword groups fully matched", groups.len())
        },
    }
}

pub fn check_output_contains_any_of(output: &str, keywords: &[String]) -> CheckResult {
    let output_lower = output.to_lowercase();
    let found: Vec<&String> = keywords
        .iter()
        .filter(|kw| output_lower.contains(&kw.to_lowercase()))
        .collect();

    CheckResult {
        name: "output_contains_any_of".to_string(),
        passed: !found.is_empty(),
        detail: if found.is_empty() {
            format!(
                "None of [{}] found in output",
                keywords
                    .iter()
                    .map(|s| format!("\"{}\"", s))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "Found: {}",
                found
                    .iter()
                    .map(|s| format!("\"{}\"", s))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    }
}

pub fn check_output_regex(output: &str, patterns: &[String]) -> Vec<CheckResult> {
    patterns
        .iter()
        .map(|pattern| match regex::Regex::new(pattern) {
            Ok(re) => {
                let found = re.is_match(output);
                CheckResult {
                    name: format!("output_regex(\"{}\")", pattern),
                    passed: found,
                    detail: if found {
                        format!("Pattern \"{}\" matched", pattern)
                    } else {
                        format!("Pattern \"{}\" did NOT match", pattern)
                    },
                }
            }
            Err(e) => CheckResult {
                name: format!("output_regex(\"{}\")", pattern),
                passed: false,
                detail: format!("Invalid regex: {e}"),
            },
        })
        .collect()
}

pub fn check_tool_calls(
    recorded: &[ToolCallInfo],
    expected: &[ToolCallExpectation],
) -> Vec<CheckResult> {
    expected
        .iter()
        .map(|exp| {
            let found = recorded.iter().any(|tc| tc.name == exp.name);
            CheckResult {
                name: format!("tool_call(\"{}\")", exp.name),
                passed: found,
                detail: if found {
                    format!("Tool \"{}\" was called", exp.name)
                } else {
                    format!("Tool \"{}\" was NOT called", exp.name)
                },
            }
        })
        .collect()
}

pub fn check_no_tool_calls(recorded: &[ToolCallInfo]) -> CheckResult {
    let passed = recorded.is_empty();
    CheckResult {
        name: "no_tool_calls".to_string(),
        passed,
        detail: if passed {
            "No tool calls made (as expected)".to_string()
        } else {
            format!(
                "Expected no tool calls, but {} were made: {}",
                recorded.len(),
                recorded
                    .iter()
                    .map(|tc| tc.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    }
}

pub fn check_max_tool_calls(recorded: &[ToolCallInfo], max: usize) -> CheckResult {
    let count = recorded.len();
    CheckResult {
        name: format!("max_tool_calls({})", max),
        passed: count <= max,
        detail: if count <= max {
            format!("{} tool calls made (max {})", count, max)
        } else {
            format!("{} tool calls made, exceeds max {}", count, max)
        },
    }
}

pub fn check_min_tool_calls(recorded: &[ToolCallInfo], min: usize) -> CheckResult {
    let count = recorded.len();
    CheckResult {
        name: format!("min_tool_calls({})", min),
        passed: count >= min,
        detail: if count >= min {
            format!("{} tool calls made (min {})", count, min)
        } else {
            format!("{} tool calls made, below min {}", count, min)
        },
    }
}

// ─── Multi-dimensional LLM Judge ────────────────────────────────────

pub async fn run_llm_judge(
    provider: &dyn LlmProvider,
    input: &str,
    output: &str,
    tool_calls: &[ToolCallInfo],
    judge: &LlmJudge,
) -> (CheckResult, Vec<DimensionScore>) {
    let dimensions: Vec<&str> = judge
        .dimensions
        .as_ref()
        .map(|d| d.iter().map(|s| s.as_str()).collect())
        .unwrap_or_else(|| DEFAULT_DIMENSIONS.to_vec());

    let min_score = judge.min_score.unwrap_or(DEFAULT_MIN_SCORE);

    let tool_summary = if tool_calls.is_empty() {
        "No tools were called.".to_string()
    } else {
        tool_calls
            .iter()
            .map(|tc| format!("- {}({})", tc.name, tc.arguments))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let dims_json: String = dimensions
        .iter()
        .map(|d| format!("    \"{}\": {{\"score\": N, \"reason\": \"...\"}}", d))
        .collect::<Vec<_>>()
        .join(",\n");

    let criteria_section = if judge.criteria.is_empty() {
        "No additional criteria.".to_string()
    } else {
        judge.criteria.clone()
    };

    let prompt = format!(
        r#"You are an evaluation judge. Score an AI agent's response on multiple quality dimensions.

## Input given to the agent
{input}

## Agent's response
{output}

## Tools called by the agent
{tool_summary}

## Additional evaluation criteria
{criteria_section}

## Scoring instructions
Score each dimension from 1 to 5:
- 1: Completely fails this dimension
- 2: Major issues
- 3: Acceptable with notable flaws
- 4: Good with minor issues
- 5: Excellent

If a dimension is not applicable (e.g., tool_usage when no tools were expected), score 5.

Respond with ONLY valid JSON, no markdown fences:
{{
{dims_json}
}}"#
    );

    let messages = vec![Message::User(prompt)];

    match provider.chat(&messages, &[]).await {
        Ok(response) => {
            let content = response.content.unwrap_or_default();
            parse_judge_scores(&content, &dimensions, min_score)
        }
        Err(e) => {
            let check = CheckResult {
                name: "llm_judge".to_string(),
                passed: false,
                detail: format!("LLM judge error: {e}"),
            };
            (check, vec![])
        }
    }
}

fn parse_judge_scores(
    content: &str,
    dimensions: &[&str],
    min_score: f32,
) -> (CheckResult, Vec<DimensionScore>) {
    let json_value = match extract_json_from_response(content) {
        Ok(v) => v,
        Err(e) => {
            return (
                CheckResult {
                    name: "llm_judge".to_string(),
                    passed: false,
                    detail: format!("Failed to parse judge response: {e}"),
                },
                vec![],
            );
        }
    };

    let mut scores = Vec::new();

    for dim in dimensions {
        if let Some(dim_obj) = json_value.get(dim) {
            let score = dim_obj
                .get("score")
                .and_then(|s| s.as_u64())
                .unwrap_or(0)
                .min(5) as u8;
            let reason = dim_obj
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            scores.push(DimensionScore {
                name: dim.to_string(),
                score,
                reason,
            });
        }
    }

    let avg = if scores.is_empty() {
        0.0
    } else {
        scores.iter().map(|s| s.score as f32).sum::<f32>() / scores.len() as f32
    };

    let passed = avg >= min_score;
    let check = CheckResult {
        name: "llm_judge".to_string(),
        passed,
        detail: format!("Average score: {:.1}/5 (min: {:.1})", avg, min_score),
    };

    (check, scores)
}

// ─── Run single eval ────────────────────────────────────────────────

pub async fn run_single_eval(
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    system_prompt: String,
    eval_case: EvalCase,
) -> EvalResult {
    let start = Instant::now();

    let storage = Arc::new(RecordingStorage::new());
    let tools = build_eval_tools(&memory_dir, eval_case.expectations.mock_responses.as_ref());

    let agent = Agent::new(
        provider.clone(),
        tools,
        Some(system_prompt),
        storage.clone(),
    );

    let conversation_id = format!("eval-{}", uuid::Uuid::new_v4());
    let timeout_secs = eval_case
        .expectations
        .timeout_seconds
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let output = match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        agent.process(&conversation_id, &eval_case.input, None),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => format!("[agent error] {e}"),
        Err(_) => format!("[timeout] eval timed out after {}s", timeout_secs),
    };

    let tool_calls_made = storage.get_recorded_tool_calls().await;
    let duration_ms = start.elapsed().as_millis();

    log::info!(
        "Eval '{}' agent finished: {:?}, output_len={}",
        eval_case.id,
        duration_ms,
        output.len()
    );

    let mut checks = Vec::new();

    // Rule-based checks
    if !eval_case.expectations.output_contains.is_empty() {
        checks.extend(check_output_contains(
            &output,
            &eval_case.expectations.output_contains,
        ));
    }
    if !eval_case.expectations.output_not_contains.is_empty() {
        checks.extend(check_output_not_contains(
            &output,
            &eval_case.expectations.output_not_contains,
        ));
    }
    if !eval_case.expectations.output_any_of.is_empty() {
        checks.push(check_output_any_of(
            &output,
            &eval_case.expectations.output_any_of,
        ));
    }
    if !eval_case.expectations.output_contains_any_of.is_empty() {
        checks.push(check_output_contains_any_of(
            &output,
            &eval_case.expectations.output_contains_any_of,
        ));
    }
    if !eval_case.expectations.output_matches_regex.is_empty() {
        checks.extend(check_output_regex(
            &output,
            &eval_case.expectations.output_matches_regex,
        ));
    }
    if !eval_case.expectations.tool_calls.is_empty() {
        checks.extend(check_tool_calls(
            &tool_calls_made,
            &eval_case.expectations.tool_calls,
        ));
    }
    if eval_case.expectations.no_tool_calls {
        checks.push(check_no_tool_calls(&tool_calls_made));
    }
    if let Some(max) = eval_case.expectations.max_tool_calls {
        checks.push(check_max_tool_calls(&tool_calls_made, max));
    }
    if let Some(min) = eval_case.expectations.min_tool_calls {
        checks.push(check_min_tool_calls(&tool_calls_made, min));
    }

    // LLM judge (multi-dimensional scoring)
    let mut scores = Vec::new();
    let mut average_score = None;

    if let Some(ref judge) = eval_case.expectations.llm_judge {
        if judge.enabled {
            let (judge_check, judge_scores) =
                run_llm_judge(provider.as_ref(), &eval_case.input, &output, &tool_calls_made, judge)
                    .await;
            if !judge_scores.is_empty() {
                let avg =
                    judge_scores.iter().map(|s| s.score as f32).sum::<f32>() / judge_scores.len() as f32;
                average_score = Some(avg);
            }
            checks.push(judge_check);
            scores = judge_scores;
        }
    }

    let passed = checks.iter().all(|c| c.passed);
    log::info!(
        "Eval '{}' result: {} ({} checks, {} passed)",
        eval_case.id,
        if passed { "PASS" } else { "FAIL" },
        checks.len(),
        checks.iter().filter(|c| c.passed).count()
    );

    EvalResult {
        eval_id: eval_case.id.clone(),
        description: eval_case.description.clone(),
        passed,
        checks,
        scores,
        average_score,
        output,
        duration_ms,
    }
}

// ─── Run eval suite (parallel) ──────────────────────────────────────

pub async fn run_eval_suite(
    provider: Arc<dyn LlmProvider>,
    memory_dir: &Path,
    suite: &EvalSuite,
    filter_id: Option<&str>,
) -> EvalReport {
    let prompt_path = memory_dir
        .join("subagents")
        .join(format!("{}.md", suite.subagent));

    let system_prompt = match tokio::fs::read_to_string(&prompt_path).await {
        Ok(p) => p,
        Err(_) => {
            return EvalReport {
                subagent: suite.subagent.clone(),
                results: vec![],
                total: suite.evals.len(),
                passed: 0,
                failed: suite.evals.len(),
            };
        }
    };

    let cases: Vec<&EvalCase> = suite
        .evals
        .iter()
        .filter(|e| filter_id.is_none_or(|id| e.id == id))
        .collect();

    let total = cases.len();

    let mut join_set = tokio::task::JoinSet::new();

    for eval_case in cases {
        let provider = provider.clone();
        let memory_dir = memory_dir.to_path_buf();
        let system_prompt = system_prompt.clone();
        let eval_case = eval_case.clone();

        log::info!(
            "Spawning eval {}: {}",
            eval_case.id,
            eval_case.description
        );

        join_set.spawn(async move {
            run_single_eval(provider, memory_dir, system_prompt, eval_case).await
        });
    }

    let mut results = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(eval_result) => results.push(eval_result),
            Err(e) => log::error!("Eval task panicked: {e}"),
        }
    }

    // Sort by eval_id for stable ordering
    results.sort_by(|a, b| a.eval_id.cmp(&b.eval_id));

    let passed = results.iter().filter(|r| r.passed).count();
    let failed = results.len() - passed;

    EvalReport {
        subagent: suite.subagent.clone(),
        results,
        total,
        passed,
        failed,
    }
}

// ─── Format report ──────────────────────────────────────────────────

fn format_score_bar(score: u8) -> String {
    let filled = score.min(5) as usize;
    let empty = 5 - filled;
    format!(
        "{}{} {}/5",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        score
    )
}

pub fn format_report(report: &EvalReport) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "=== Eval Report: {} ===\n",
        report.subagent
    ));
    out.push_str(&format!(
        "Total: {} | Passed: {} | Failed: {}\n\n",
        report.total, report.passed, report.failed
    ));

    for result in &report.results {
        let status = if result.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "[{}] {} \u{2014} {} ({}ms)\n",
            status, result.eval_id, result.description, result.duration_ms
        ));

        for check in &result.checks {
            let check_status = if check.passed { "+" } else { "-" };
            out.push_str(&format!(
                "  [{}] {}: {}\n",
                check_status, check.name, check.detail
            ));
        }

        // Dimension scores
        if !result.scores.is_empty() {
            out.push_str("  Scores:\n");

            let max_name_len = result
                .scores
                .iter()
                .map(|s| s.name.len())
                .max()
                .unwrap_or(0);

            for s in &result.scores {
                out.push_str(&format!(
                    "    {:<width$}  {}  {}\n",
                    s.name,
                    format_score_bar(s.score),
                    s.reason,
                    width = max_name_len
                ));
            }

            if let Some(avg) = result.average_score {
                out.push_str(&format!(
                    "    {}\n    Average: {:.1}/5\n",
                    "\u{2500}".repeat(max_name_len + 16),
                    avg
                ));
            }
        }

        if !result.passed {
            let preview: String = result.output.chars().take(200).collect();
            let truncated = result.output.chars().count() > 200;
            if truncated {
                out.push_str(&format!("  Output preview: {}...\n", preview));
            } else {
                out.push_str(&format!("  Output preview: {}\n", preview));
            }
        }

        out.push('\n');
    }

    out
}

// ─── Stability report (for --runs) ──────────────────────────────────

pub fn format_stability_report(
    runs: u32,
    stability: &HashMap<String, (u32, u32)>,
) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "\n=== Stability Report ({} runs) ===\n",
        runs
    ));

    let max_id_len = stability.keys().map(|k| k.len()).max().unwrap_or(20);

    let mut entries: Vec<_> = stability.iter().collect();
    entries.sort_by_key(|(id, _)| (*id).clone());

    for (id, (passed, total)) in entries {
        let label = if *passed == *total {
            "STABLE"
        } else if *passed == 0 {
            "FAILING"
        } else {
            "FLAKY"
        };
        out.push_str(&format!(
            "  {:<width$}  {}/{} {}\n",
            id,
            passed,
            total,
            label,
            width = max_id_len
        ));
    }

    out
}

// ─── Result persistence ─────────────────────────────────────────────

pub async fn save_eval_report(memory_dir: &Path, report: &EvalReport) -> Result<(), String> {
    let results_dir = memory_dir.join("eval_results");
    tokio::fs::create_dir_all(&results_dir)
        .await
        .map_err(|e| format!("Cannot create eval_results directory: {e}"))?;

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("{}_{}.json", report.subagent, timestamp);
    let path = results_dir.join(filename);

    let json = serde_json::to_string_pretty(report)
        .map_err(|e| format!("Cannot serialize eval report: {e}"))?;

    tokio::fs::write(&path, &json)
        .await
        .map_err(|e| format!("Cannot write eval report: {e}"))?;

    log::info!("Eval report saved to {}", path.display());
    Ok(())
}

// ─── Extract JSON from LLM response ────────────────────────────────

pub fn extract_json_from_response(response: &str) -> Result<serde_json::Value, String> {
    // Try direct parse first
    if let Ok(v) = serde_json::from_str(response) {
        return Ok(v);
    }

    // Try to extract from ```json ... ``` block
    if let Some(start) = response.find("```json") {
        let json_start = start + 7;
        if let Some(end) = response[json_start..].find("```") {
            let json_str = response[json_start..json_start + end].trim();
            return serde_json::from_str(json_str)
                .map_err(|e| format!("Failed to parse JSON from code block: {e}"));
        }
    }

    // Try to extract from ``` ... ``` block (without json tag)
    if let Some(start) = response.find("```") {
        let json_start = start + 3;
        // Skip optional language tag on same line
        let json_start = if let Some(nl) = response[json_start..].find('\n') {
            json_start + nl + 1
        } else {
            json_start
        };
        if let Some(end) = response[json_start..].find("```") {
            let json_str = response[json_start..json_start + end].trim();
            if let Ok(v) = serde_json::from_str(json_str) {
                return Ok(v);
            }
        }
    }

    // Try to find JSON object boundaries
    if let Some(start) = response.find('{') {
        if let Some(end) = response.rfind('}') {
            let json_str = &response[start..=end];
            if let Ok(v) = serde_json::from_str(json_str) {
                return Ok(v);
            }
        }
    }

    Err("Could not extract valid JSON from response".to_string())
}

// ─── Load eval suites from disk ─────────────────────────────────────

pub fn load_eval_suite(memory_dir: &Path, agent_name: &str) -> Result<EvalSuite, String> {
    let path = memory_dir
        .join("evals")
        .join(format!("{agent_name}.json"));
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&content)
        .map_err(|e| format!("Invalid eval JSON for {agent_name}: {e}"))
}

pub fn list_eval_files(memory_dir: &Path) -> Vec<String> {
    let evals_dir = memory_dir.join("evals");
    let entries = match std::fs::read_dir(&evals_dir) {
        Ok(entries) => entries,
        Err(_) => return vec![],
    };

    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") {
                Some(name.trim_end_matches(".json").to_string())
            } else {
                None
            }
        })
        .collect();

    names.sort();
    names
}
