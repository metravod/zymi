use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

pub type SharedTaskRegistry = Arc<RwLock<TaskRegistry>>;

pub fn new_task_registry() -> SharedTaskRegistry {
    Arc::new(RwLock::new(TaskRegistry::new()))
}

#[derive(Debug, Clone)]
pub enum TaskKind {
    Agent { agent_name: String },
    Shell { command: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Pending => write!(f, "pending"),
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Completed => write!(f, "completed"),
            TaskStatus::Failed => write!(f, "failed"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub id: String,
    pub kind: TaskKind,
    pub description: String,
    pub status: TaskStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_at: Instant,
    pub completed_at: Option<Instant>,
}

impl TaskEntry {
    pub fn new(id: String, kind: TaskKind, description: String) -> Self {
        Self {
            id,
            kind,
            description,
            status: TaskStatus::Pending,
            result: None,
            error: None,
            created_at: Instant::now(),
            completed_at: None,
        }
    }

    pub fn elapsed_secs(&self) -> f64 {
        match self.completed_at {
            Some(t) => t.duration_since(self.created_at).as_secs_f64(),
            None => self.created_at.elapsed().as_secs_f64(),
        }
    }
}

pub struct TaskRegistry {
    tasks: HashMap<String, TaskEntry>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }

    pub fn insert(&mut self, entry: TaskEntry) {
        self.tasks.insert(entry.id.clone(), entry);
    }

    pub fn get(&self, id: &str) -> Option<&TaskEntry> {
        self.tasks.get(id)
    }

    pub fn set_running(&mut self, id: &str) {
        if let Some(entry) = self.tasks.get_mut(id) {
            entry.status = TaskStatus::Running;
        }
    }

    pub fn set_completed(&mut self, id: &str, result: String) {
        if let Some(entry) = self.tasks.get_mut(id) {
            entry.status = TaskStatus::Completed;
            entry.result = Some(result);
            entry.completed_at = Some(Instant::now());
        }
    }

    pub fn set_failed(&mut self, id: &str, error: String) {
        if let Some(entry) = self.tasks.get_mut(id) {
            entry.status = TaskStatus::Failed;
            entry.error = Some(error);
            entry.completed_at = Some(Instant::now());
        }
    }

    pub fn list(&self) -> Vec<&TaskEntry> {
        let mut entries: Vec<_> = self.tasks.values().collect();
        entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        entries
    }

    pub fn active_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::Running))
            .count()
    }
}
