use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;

use crate::audit::AuditLog;
use crate::connectors::telegram::allowed_users;
use crate::core::agent::Agent;
use crate::core::LlmProvider;
use crate::policy::PolicyEngine;
use crate::storage::in_memory::InMemoryStorage;
use crate::tools::current_time::CurrentTimeTool;
use crate::tools::memory::{ReadMemoryTool, WriteMemoryTool};
use crate::tools::shell::ShellTool;
use crate::tools::Tool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleEntry {
    pub id: String,
    pub name: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub once_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub fn load_schedule(memory_dir: &Path) -> Vec<ScheduleEntry> {
    let path = memory_dir.join("schedule.json");
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            log::error!("Failed to parse schedule.json: {e}");
            vec![]
        }),
        Err(_) => vec![],
    }
}

pub fn save_schedule(memory_dir: &Path, entries: &[ScheduleEntry]) {
    let path = memory_dir.join("schedule.json");
    let tmp_path = memory_dir.join("schedule.json.tmp");

    let content = match serde_json::to_string_pretty(entries) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to serialize schedule: {e}");
            return;
        }
    };

    if let Err(e) = std::fs::write(&tmp_path, &content) {
        log::error!("Failed to write temp schedule file: {e}");
        return;
    }

    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        log::error!("Failed to rename schedule file: {e}");
    }
}

/// Parse a standard 5-field cron expression by padding it to the 7-field format
/// required by the `cron` crate (sec + 5 fields + year).
pub fn parse_cron(expr: &str) -> Result<Schedule, String> {
    let padded = format!("0 {expr} *");
    Schedule::from_str(&padded).map_err(|e| format!("Invalid cron expression '{expr}': {e}"))
}

fn should_fire(entry: &ScheduleEntry, now: DateTime<Utc>) -> bool {
    if let Some(ref cron_expr) = entry.cron {
        let schedule = match parse_cron(cron_expr) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Bad cron in entry '{}': {e}", entry.id);
                return false;
            }
        };
        let after = entry.last_run.unwrap_or(entry.created_at);
        match schedule.after(&after).next() {
            Some(next_fire) => next_fire <= now,
            None => false,
        }
    } else if let Some(once_at) = entry.once_at {
        once_at <= now && entry.last_run.is_none()
    } else {
        false
    }
}

async fn execute_entry(
    entry: &ScheduleEntry,
    provider: &Arc<dyn LlmProvider>,
    memory_dir: &Path,
    system_prompt: &str,
    policy_engine: &Arc<PolicyEngine>,
    audit: &AuditLog,
) -> String {
    log::info!(
        "Executing scheduled task '{}' ({}): {}",
        entry.name,
        entry.id,
        entry.task
    );
    let start = std::time::Instant::now();

    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(CurrentTimeTool),
        Box::new(ReadMemoryTool::new(memory_dir.to_path_buf())),
        Box::new(WriteMemoryTool::new(memory_dir.to_path_buf())),
        Box::new(ShellTool::new().with_policy(policy_engine.clone())),
    ];

    let storage = Arc::new(InMemoryStorage::new());
    let agent = Agent::new(
        provider.clone(),
        tools,
        Some(system_prompt.to_string()),
        storage,
    )
    .with_audit(audit.clone());

    let conversation_id = format!("scheduled-{}", uuid::Uuid::new_v4());

    match agent.process(&conversation_id, &entry.task, None).await {
        Ok(response) => {
            log::info!(
                "Scheduled task '{}' completed: {:?}, response_len={}",
                entry.name,
                start.elapsed(),
                response.len()
            );
            response
        }
        Err(e) => {
            log::error!(
                "Scheduled task '{}' failed: {:?}, error: {e}",
                entry.name,
                start.elapsed()
            );
            format!("Scheduled task '{}' error: {e}", entry.name)
        }
    }
}

async fn notify_users(bot: &Bot, message: &str) {
    let users = match allowed_users() {
        Ok(u) => u,
        Err(e) => {
            log::error!("Cannot notify users: {e}");
            return;
        }
    };
    for user_id in users {
        let chat_id = ChatId(user_id.0 as i64);
        if let Err(e) = bot.send_message(chat_id, message).await {
            log::error!("Failed to notify user {user_id}: {e}");
        }
    }
}

pub async fn run(
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    system_prompt: String,
    policy_engine: Arc<PolicyEngine>,
    audit: AuditLog,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let bot = crate::connectors::telegram::bot_with_timeout();
    log::info!("Scheduler started");

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {}
            _ = shutdown.cancelled() => {
                log::info!("Scheduler: shutdown signal received");
                return;
            }
        }

        let mut entries = load_schedule(&memory_dir);
        if entries.is_empty() {
            continue;
        }

        let now = Utc::now();
        let mut changed = false;
        log::debug!("Scheduler tick: checking {} entries at {}", entries.len(), now);

        for entry in entries.iter_mut() {
            if !should_fire(entry, now) {
                continue;
            }

            log::info!("Firing scheduled task '{}' ({})", entry.name, entry.id);

            let response = execute_entry(
                entry, &provider, &memory_dir, &system_prompt,
                &policy_engine, &audit,
            ).await;

            let message = format!("⏰ [{}]\n\n{}", entry.name, response);
            notify_users(&bot, &message).await;

            entry.last_run = Some(now);
            changed = true;
        }

        // Remove fired one-time entries
        let before_len = entries.len();
        entries.retain(|e| !(e.once_at.is_some() && e.last_run.is_some()));
        if entries.len() != before_len {
            changed = true;
        }

        if changed {
            save_schedule(&memory_dir, &entries);
        }
    }
}
