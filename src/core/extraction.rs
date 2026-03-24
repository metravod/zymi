use std::sync::Arc;

use crate::core::{LlmProvider, Message};

const EXTRACT_FACTS_PROMPT: &str = "\
You are analyzing a single user message in the context of an ongoing conversation. \
Extract any durable, interesting facts that would be worth remembering across future conversations.\n\n\
Focus on:\n\
- Personal facts: names, relationships, locations, birthdays, pets\n\
- Professional facts: projects, employers, clients, deadlines, tech stack\n\
- Preferences and opinions beyond communication style (favorite restaurants, hobbies, brands)\n\
- Important decisions, plans, goals, or commitments the user mentions\n\
- Domain knowledge: specific terms, systems, processes the user refers to\n\
- Any concrete data points (server IPs, project names, repo URLs, account names)\n\n\
Rules:\n\
- Return one fact per line, as a bullet point (- fact)\n\
- Each fact should be self-contained and understandable without conversation context\n\
- Skip anything that is task-specific instructions to you (e.g. \"fix this bug\" is not a fact)\n\
- Skip things already present in the existing facts list provided below\n\
- If no new durable facts are found, respond with exactly: NO_FACTS\n\
- Write facts in the same language as the user's message.";

pub(crate) fn is_trivial_reply(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const TRIVIAL: &[&str] = &[
        "да", "нет", "ок", "ok", "yes", "no", "ага", "угу",
        "спасибо", "thanks", "thank you", "good", "хорошо", "понял",
        "ладно", "давай", "go", "got it", "👍", "👎", "+", "-",
        "продолжай", "continue", "далее", "next", "дальше",
    ];
    TRIVIAL.iter().any(|t| lower == *t)
}

/// Background task: extract durable facts from a user message.
pub(super) async fn extract_facts(
    provider: Arc<dyn LlmProvider>,
    memory_dir: &std::path::Path,
    user_message: &str,
) {
    let facts_path = memory_dir.join("facts.md");
    let existing_facts = std::fs::read_to_string(&facts_path).unwrap_or_default();

    let mut input = String::new();
    if !existing_facts.trim().is_empty() {
        input.push_str("[Existing facts — do NOT repeat these]\n");
        input.push_str(&existing_facts);
        input.push_str("\n\n");
    }
    input.push_str("[User message to analyze]\n");
    input.push_str(user_message);

    let messages = vec![
        Message::System(EXTRACT_FACTS_PROMPT.to_string()),
        Message::User(input),
    ];

    match provider.chat(&messages, &[]).await {
        Ok(response) => {
            if let Some(facts) = response.content {
                let trimmed = facts.trim();
                if trimmed == "NO_FACTS" || trimmed.is_empty() {
                    log::debug!("Auto-extract: no new facts found");
                    return;
                }

                let timestamp = chrono::Utc::now().format("%Y-%m-%d").to_string();
                let entry = format!("\n## {timestamp}\n{trimmed}\n");

                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&facts_path)
                {
                    Ok(mut file) => {
                        use std::io::Write;
                        if let Err(e) = file.write_all(entry.as_bytes()) {
                            log::warn!("Auto-extract: failed to write facts: {e}");
                        } else {
                            log::info!("Auto-extract: saved new facts ({} chars)", trimmed.len());
                        }
                    }
                    Err(e) => {
                        log::warn!("Auto-extract: failed to open facts.md: {e}");
                    }
                }
            }
        }
        Err(e) => {
            log::warn!("Auto-extract: LLM call failed: {e}");
        }
    }
}
