use std::collections::HashSet;
use std::sync::Arc;

use crate::core::{ContentPart, LlmProvider, Message};
use crate::storage::ConversationStorage;

pub(crate) const KEEP_RECENT_MESSAGES: usize = 20;

const SUMMARY_PROMPT: &str = "\
Summarize the following conversation between a user and an AI assistant. \
Extract and preserve:\n\
- Key decisions and agreements\n\
- Important facts, names, URLs, and technical details\n\
- Current task status and pending items\n\n\
Be concise but complete. This summary replaces the conversation history \
and will be the only context available for future interactions.\n\
Do NOT include user preferences or personality observations here — \
those are extracted separately.";

const PREFERENCES_PROMPT: &str = "\
You are analyzing a conversation to extract lasting insights about the user. \
Focus ONLY on things that will remain relevant across future conversations.\n\n\
Extract:\n\
- Communication style and language preferences (language, formality, brevity)\n\
- Domain expertise and professional context\n\
- Recurring interests, hobbies, personal facts\n\
- Workflow preferences (tools, formats, approaches they prefer)\n\
- Pet peeves, things they dislike or explicitly asked to avoid\n\
- Important names, places, projects they reference regularly\n\n\
Rules:\n\
- Be concise: one line per insight, use bullet points\n\
- Skip anything task-specific or temporary\n\
- Skip obvious things (e.g. \"user asks questions\" — of course they do)\n\
- If the conversation reveals nothing new about the user, respond with exactly: NO_UPDATE\n\
- If previous preferences are provided, merge new insights into them: \
update existing items if new info refines them, add new items, \
remove items that are clearly contradicted. Return the full updated list.\n\
- Write in the same language the user primarily speaks in the conversation.";

const CONSOLIDATE_FACTS_PROMPT: &str = "\
You are consolidating a list of facts about a user. \
Merge duplicates, remove contradicted facts (keep the newer version), \
and organize into logical groups (personal, professional, technical, etc.).\n\n\
Rules:\n\
- One fact per line, bullet point format\n\
- Remove date headers, just keep the clean facts\n\
- Preserve all unique, non-contradicted information\n\
- Write in the same language as the original facts.";

/// Sanitize conversation history by:
/// 1. Removing orphaned ToolResults whose tool_call is missing (e.g. after summarization)
/// 2. Removing trailing Assistant messages with tool_calls that lack corresponding results
///    (e.g. after a crash between saving assistant message and tool results)
pub(crate) fn sanitize_history(messages: &mut Vec<Message>) {
    // Pass 1: remove orphaned ToolResults (tool_call_id not found in any Assistant)
    let all_call_ids: HashSet<String> = messages
        .iter()
        .filter_map(|m| {
            if let Message::Assistant { tool_calls, .. } = m {
                Some(tool_calls.iter().map(|tc| tc.id.clone()))
            } else {
                None
            }
        })
        .flatten()
        .collect();

    let before = messages.len();
    messages.retain(|m| {
        if let Message::ToolResult { tool_call_id, .. } = m {
            all_call_ids.contains(tool_call_id)
        } else {
            true
        }
    });
    if messages.len() < before {
        log::warn!(
            "Sanitizing history: removed {} orphaned tool result(s)",
            before - messages.len()
        );
    }

    // Pass 2: remove Assistant messages with missing tool results.
    // Only remove the broken assistant and its orphaned ToolResults — keep any
    // subsequent User/UserMultimodal messages intact.  The old `truncate(idx)`
    // wiped everything after the broken assistant, which destroyed new user
    // messages (e.g. a photo) that arrived while ask_user was pending.
    loop {
        let last_assistant_idx = messages.iter().rposition(|m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()));

        let Some(idx) = last_assistant_idx else {
            break;
        };

        let expected_ids: HashSet<String> = if let Message::Assistant { tool_calls, .. } = &messages[idx] {
            tool_calls.iter().map(|tc| tc.id.clone()).collect()
        } else {
            break;
        };

        let actual_ids: HashSet<String> = messages[idx + 1..]
            .iter()
            .filter_map(|m| {
                if let Message::ToolResult { tool_call_id, .. } = m {
                    Some(tool_call_id.clone())
                } else {
                    None
                }
            })
            .collect();

        if expected_ids.is_subset(&actual_ids) {
            break;
        }

        let missing: Vec<_> = expected_ids.difference(&actual_ids).collect();
        log::warn!(
            "Sanitizing history: removing assistant message at index {} with {} orphaned tool_call(s): {:?}",
            idx,
            missing.len(),
            missing
        );

        // Collect IDs of the broken assistant's tool calls for ToolResult cleanup
        let broken_ids: HashSet<String> = expected_ids;

        // Remove the assistant message itself
        messages.remove(idx);

        // Remove any ToolResults that belonged to this broken assistant
        messages.retain(|m| {
            if let Message::ToolResult { tool_call_id, .. } = m {
                !broken_ids.contains(tool_call_id)
            } else {
                true
            }
        });
    }
}

/// Background task: summarize a conversation if it exceeds the threshold.
/// Uses sliding window: only older messages are summarized, recent ones are kept intact.
pub(super) async fn summarize_conversation(
    provider: Arc<dyn LlmProvider>,
    storage: Arc<dyn ConversationStorage>,
    memory_dir: &std::path::Path,
    conversation_id: &str,
    summary_threshold: usize,
) {
    let history = match storage.get_history(conversation_id).await {
        Ok(h) => h,
        Err(_) => return,
    };

    if history.len() < summary_threshold {
        return;
    }

    // Split: summarize the older part, keep the recent part.
    // Adjust split point so we never orphan a ToolResult from its tool_call.
    let mut split_point = history.len().saturating_sub(KEEP_RECENT_MESSAGES);
    while split_point > 0 {
        if let Message::ToolResult { .. } = &history[split_point] {
            split_point -= 1;
        } else {
            break;
        }
    }
    let to_summarize = &history[..split_point];
    let to_keep: Vec<Message> = history[split_point..].to_vec();

    log::info!(
        "Conversation '{}' has {} messages, summarizing (keeping last {})...",
        conversation_id,
        history.len(),
        to_keep.len(),
    );

    let summary_dir = memory_dir.join("conversations");
    let summary_path = summary_dir.join(format!("{conversation_id}.md"));
    let prefs_path = memory_dir.join("preferences.md");

    // Include previous summary so we don't lose older context
    let previous = std::fs::read_to_string(&summary_path).unwrap_or_default();

    let formatted_history = format_history_for_summary(to_summarize);

    // Build summary input
    let mut summary_input = String::new();
    if !previous.trim().is_empty() {
        summary_input.push_str("[Previous context summary]\n");
        summary_input.push_str(&previous);
        summary_input.push_str("\n\n[New conversation to incorporate]\n");
    }
    summary_input.push_str(&formatted_history);

    let summary_messages = vec![
        Message::System(SUMMARY_PROMPT.to_string()),
        Message::User(summary_input),
    ];

    // Build preferences input
    let existing_prefs = std::fs::read_to_string(&prefs_path).unwrap_or_default();
    let mut prefs_input = String::new();
    if !existing_prefs.trim().is_empty() {
        prefs_input.push_str("[Existing user preferences]\n");
        prefs_input.push_str(&existing_prefs);
        prefs_input.push_str("\n\n[New conversation to analyze]\n");
    }
    prefs_input.push_str(&formatted_history);

    let prefs_messages = vec![
        Message::System(PREFERENCES_PROMPT.to_string()),
        Message::User(prefs_input),
    ];

    // Run both LLM calls in parallel
    let provider2 = provider.clone();
    let (summary_result, prefs_result) = tokio::join!(
        provider.chat(&summary_messages, &[]),
        provider2.chat(&prefs_messages, &[]),
    );

    // Write summary and re-add recent messages
    match summary_result {
        Ok(response) => {
            if let Some(summary) = response.content {
                let _ = std::fs::create_dir_all(&summary_dir);
                if std::fs::write(&summary_path, &summary).is_ok() {
                    let _ = storage.clear(conversation_id).await;
                    // Re-add recent messages to preserve active context
                    for msg in &to_keep {
                        if let Err(e) = storage.add_message(conversation_id, msg).await {
                            log::warn!("Failed to re-add kept message: {e}");
                        }
                    }
                    log::info!(
                        "Conversation '{}' summarized: removed {} old messages, kept {}",
                        conversation_id,
                        split_point,
                        to_keep.len(),
                    );
                } else {
                    log::warn!("Failed to write summary file for '{}'", conversation_id);
                }
            }
        }
        Err(e) => {
            log::warn!("Failed to summarize '{}': {e}", conversation_id);
        }
    }

    // Write preferences
    match prefs_result {
        Ok(response) => {
            if let Some(prefs) = response.content {
                let trimmed = prefs.trim();
                if trimmed == "NO_UPDATE" || trimmed.is_empty() {
                    log::info!("No new user preferences extracted");
                } else if std::fs::write(&prefs_path, &prefs).is_ok() {
                    log::info!("User preferences updated ({} chars)", prefs.len());
                } else {
                    log::warn!("Failed to write preferences file");
                }
            }
        }
        Err(e) => {
            log::warn!("Failed to extract preferences: {e}");
        }
    }

    // Consolidate facts if they've grown large
    let facts_path = memory_dir.join("facts.md");
    if let Ok(facts_content) = std::fs::read_to_string(&facts_path) {
        if facts_content.len() > 6000 {
            log::info!("Facts file large ({} chars), consolidating...", facts_content.len());
            let consolidate_messages = vec![
                Message::System(CONSOLIDATE_FACTS_PROMPT.to_string()),
                Message::User(facts_content),
            ];
            if let Ok(response) = provider.chat(&consolidate_messages, &[]).await {
                if let Some(consolidated) = response.content {
                    let out = format!("# User Facts\n\n{}\n", consolidated.trim());
                    if std::fs::write(&facts_path, &out).is_ok() {
                        log::info!("Facts consolidated ({} chars)", consolidated.len());
                    }
                }
            }
        }
    }
}

fn format_history_for_summary(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        match msg {
            Message::System(_) => {}
            Message::User(content) => {
                parts.push(format!("User: {}", truncate_for_summary(content, 500)));
            }
            Message::UserMultimodal { parts: msg_parts } => {
                let text: String = msg_parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let label = if msg_parts.iter().any(|p| matches!(p, ContentPart::ImageBase64 { .. })) {
                    "User [with image]"
                } else {
                    "User"
                };
                parts.push(format!("{label}: {}", truncate_for_summary(&text, 500)));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(c) = content {
                    parts.push(format!("Assistant: {}", truncate_for_summary(c, 500)));
                }
                for tc in tool_calls {
                    parts.push(format!("[Used tool: {}]", tc.name));
                }
            }
            Message::ToolResult { content, .. } => {
                parts.push(format!(
                    "[Tool output: {}]",
                    truncate_for_summary(content, 150)
                ));
            }
        }
    }
    let text = parts.join("\n");
    if text.len() > 8000 {
        let end = text.floor_char_boundary(8000);
        format!("{}...\n\n[Truncated]", &text[..end])
    } else {
        text
    }
}

fn truncate_for_summary(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}
