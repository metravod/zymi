use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::core::Message;
use crate::storage::{ConversationStorage, StorageError};

pub struct SqliteStorage {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStorage {
    pub fn new(path: &Path) -> Result<Self, StorageError> {
        let conn =
            Connection::open(path).map_err(|e| StorageError::Connection(e.to_string()))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )
        .map_err(|e| StorageError::Connection(e.to_string()))?;

        // Create new normalized schema
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 conversation_id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_messages_conv_seq
                 ON messages(conversation_id, seq);",
        )
        .map_err(|e| StorageError::Connection(e.to_string()))?;

        // Migrate from old schema if it exists
        migrate_from_blob(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

/// Migrate from the old `conversations(id, messages JSON blob)` table
/// to the new normalized `messages` table. Runs once, then drops the old table.
fn migrate_from_blob(conn: &Connection) -> Result<(), StorageError> {
    // Check if old table exists
    let old_exists: bool = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='conversations'")
        .and_then(|mut s| s.query_row([], |_| Ok(true)))
        .unwrap_or(false);

    if !old_exists {
        return Ok(());
    }

    let count: i64 = conn
        .prepare("SELECT COUNT(*) FROM conversations")
        .and_then(|mut s| s.query_row([], |row| row.get(0)))
        .unwrap_or(0);

    if count == 0 {
        conn.execute("DROP TABLE conversations", [])
            .map_err(|e| StorageError::Connection(e.to_string()))?;
        return Ok(());
    }

    log::info!("Migrating {count} conversations from blob to normalized schema...");

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| StorageError::Connection(e.to_string()))?;

    {
        let mut stmt = tx
            .prepare("SELECT id, messages FROM conversations")
            .map_err(|e| StorageError::Connection(e.to_string()))?;

        let mut insert = tx
            .prepare("INSERT INTO messages (conversation_id, seq, data) VALUES (?1, ?2, ?3)")
            .map_err(|e| StorageError::Connection(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let blob: String = row.get(1)?;
                Ok((id, blob))
            })
            .map_err(|e| StorageError::Connection(e.to_string()))?;

        for row in rows {
            let (conv_id, blob) = row.map_err(|e| StorageError::Connection(e.to_string()))?;

            let messages: Vec<serde_json::Value> = match serde_json::from_str(&blob) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("Skipping conversation {conv_id}: invalid JSON: {e}");
                    continue;
                }
            };

            for (seq, msg) in messages.iter().enumerate() {
                let data = serde_json::to_string(msg)
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                insert
                    .execute(rusqlite::params![conv_id, seq as i64, data])
                    .map_err(|e| StorageError::Connection(e.to_string()))?;
            }
        }
    }

    tx.execute("DROP TABLE conversations", [])
        .map_err(|e| StorageError::Connection(e.to_string()))?;

    tx.commit()
        .map_err(|e| StorageError::Connection(e.to_string()))?;

    log::info!("Migration complete.");
    Ok(())
}

#[async_trait]
impl ConversationStorage for SqliteStorage {
    async fn get_history(&self, conversation_id: &str) -> Result<Vec<Message>, StorageError> {
        let conn = self.conn.clone();
        let id = conversation_id.to_owned();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT data FROM messages WHERE conversation_id = ?1 ORDER BY seq ASC",
                )
                .map_err(|e| StorageError::Connection(e.to_string()))?;

            let rows = stmt
                .query_map([&id], |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .map_err(|e| StorageError::Connection(e.to_string()))?;

            let mut messages = Vec::new();
            for row in rows {
                let data = row.map_err(|e| StorageError::Connection(e.to_string()))?;
                let msg: Message = serde_json::from_str(&data)
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                messages.push(msg);
            }
            Ok(messages)
        })
        .await
        .map_err(|e| StorageError::Connection(e.to_string()))?
    }

    async fn add_message(
        &self,
        conversation_id: &str,
        message: &Message,
    ) -> Result<(), StorageError> {
        let conn = self.conn.clone();
        let id = conversation_id.to_owned();
        let data = serde_json::to_string(message)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            // Get next seq number
            let next_seq: i64 = conn
                .prepare("SELECT COALESCE(MAX(seq), -1) + 1 FROM messages WHERE conversation_id = ?1")
                .and_then(|mut s| s.query_row([&id], |row| row.get(0)))
                .map_err(|e| StorageError::Connection(e.to_string()))?;

            conn.execute(
                "INSERT INTO messages (conversation_id, seq, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, next_seq, data],
            )
            .map_err(|e| StorageError::Connection(e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| StorageError::Connection(e.to_string()))?
    }

    async fn clear(&self, conversation_id: &str) -> Result<(), StorageError> {
        let conn = self.conn.clone();
        let id = conversation_id.to_owned();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute("DELETE FROM messages WHERE conversation_id = ?1", [&id])
                .map_err(|e| StorageError::Connection(e.to_string()))?;
            Ok(())
        })
        .await
        .map_err(|e| StorageError::Connection(e.to_string()))?
    }
}
