use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::core::Message;
use crate::storage::{ConversationStorage, StorageError};

pub struct InMemoryStorage {
    data: Mutex<HashMap<String, Vec<Message>>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl ConversationStorage for InMemoryStorage {
    async fn get_history(&self, conversation_id: &str) -> Result<Vec<Message>, StorageError> {
        let data = self.data.lock().await;
        Ok(data.get(conversation_id).cloned().unwrap_or_default())
    }

    async fn add_message(
        &self,
        conversation_id: &str,
        message: &Message,
    ) -> Result<(), StorageError> {
        let mut data = self.data.lock().await;
        data.entry(conversation_id.to_string())
            .or_default()
            .push(message.clone());
        Ok(())
    }

    async fn clear(&self, conversation_id: &str) -> Result<(), StorageError> {
        let mut data = self.data.lock().await;
        data.remove(conversation_id);
        Ok(())
    }
}
