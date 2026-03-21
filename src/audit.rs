use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub event: AuditEvent,
    pub conversation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // Variants used as the feature set grows
pub enum AuditEvent {
    ToolCall {
        tool: String,
        arguments: String,
        result_preview: String,
        is_error: bool,
    },
    ShellCommand {
        command: String,
        exit_code: Option<i32>,
        policy_decision: String,
    },
    ApprovalRequest {
        tool: String,
        description: String,
        approved: bool,
    },
    AgentStart {
        model: String,
    },
    AgentStop,
}

/// Async audit logger. Writes append-only JSONL to a file.
pub struct AuditLog {
    tx: mpsc::UnboundedSender<AuditEntry>,
}

impl AuditLog {
    /// Create a new audit log that writes to `{memory_dir}/audit.jsonl`.
    /// Returns the logger and a JoinHandle for the background writer.
    pub fn new(memory_dir: &Path) -> Self {
        let path = memory_dir.join("audit.jsonl");
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(writer_loop(path, rx));

        Self { tx }
    }

    pub fn log(&self, event: AuditEvent) {
        self.log_with_conversation(event, None);
    }

    pub fn log_with_conversation(&self, event: AuditEvent, conversation_id: Option<String>) {
        let entry = AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            event,
            conversation_id,
        };
        // Ignore send errors (logger may have been dropped)
        let _ = self.tx.send(entry);
    }
}

impl Clone for AuditLog {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

async fn writer_loop(path: PathBuf, mut rx: mpsc::UnboundedReceiver<AuditEntry>) {
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            log::error!("Audit log: failed to open {}: {e}", path.display());
            return;
        }
    };

    while let Some(entry) = rx.recv().await {
        let line = match serde_json::to_string(&entry) {
            Ok(json) => format!("{json}\n"),
            Err(e) => {
                log::error!("Audit log: failed to serialize entry: {e}");
                continue;
            }
        };

        if let Err(e) = file.write_all(line.as_bytes()).await {
            log::error!("Audit log: failed to write: {e}");
        }
    }
}
