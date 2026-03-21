pub mod in_memory;
pub mod sqlite_storage;

use async_trait::async_trait;
use thiserror::Error;

use crate::core::Message;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

#[async_trait]
#[allow(dead_code)]
pub trait ConversationStorage: Send + Sync {
    async fn get_history(&self, conversation_id: &str) -> Result<Vec<Message>, StorageError>;
    async fn add_message(&self, conversation_id: &str, message: &Message)
        -> Result<(), StorageError>;
    async fn clear(&self, conversation_id: &str) -> Result<(), StorageError>;
}
