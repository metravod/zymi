use std::sync::Arc;

use serde::Deserialize;

use crate::core::{LlmProvider, Message};

use super::WorkflowError;

const ASSESSMENT_PROMPT: &str = "\
You are a task complexity assessor. Evaluate the user's request and rate its complexity on a scale of 1-10.

Score guidelines:
1-3: Simple, direct tasks. Single-step answers, factual lookups, basic questions, casual conversation.
     Examples: \"What time is it?\", \"Translate this word\", \"How are you?\", \"Set a reminder\"
4-6: Moderate tasks requiring several steps or a couple of tools.
     Examples: \"Search for X and summarize\", \"Write a short function\", \"Compare A and B\"
7-10: Complex tasks requiring research, planning, multiple tools, code generation, or multi-step reasoning.
      Examples: \"Build a module for X\", \"Analyze this data and create a report\", \
\"Research X, compare approaches, and implement the best one\"

Respond with ONLY valid JSON, no markdown fences:
{\"score\": <1-10>, \"reasoning\": \"<brief explanation>\", \"suggested_approach\": \"<high-level approach if score > 3>\"}";

#[derive(Debug, Clone, Deserialize)]
pub struct Assessment {
    pub score: u8,
    pub reasoning: String,
    #[serde(default)]
    pub suggested_approach: String,
}

/// Fast heuristic pre-filter: returns `Some(Assessment)` for obviously simple
/// messages without making an LLM call. Returns `None` when LLM is needed.
pub fn quick_assess(user_message: &str) -> Option<Assessment> {
    let trimmed = user_message.trim();
    let lower = trimmed.to_lowercase();
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    let word_count = words.len();

    // Very short messages (≤3 words) → score 1
    if word_count <= 3 {
        return Some(Assessment {
            score: 1,
            reasoning: "very short message".to_string(),
            suggested_approach: String::new(),
        });
    }

    // Greetings with ≤10 words → score 1
    let greetings = [
        "привет", "hello", "hi", "hey", "добрый день", "добрый вечер",
        "доброе утро", "здравствуйте", "good morning", "good evening",
        "good afternoon",
    ];
    if word_count <= 10 && greetings.iter().any(|g| lower.starts_with(g)) {
        return Some(Assessment {
            score: 1,
            reasoning: "greeting".to_string(),
            suggested_approach: String::new(),
        });
    }

    // Short factual questions (ends with ?, ≤8 words) → score 2
    if trimmed.ends_with('?') && word_count <= 8 {
        return Some(Assessment {
            score: 2,
            reasoning: "short factual question".to_string(),
            suggested_approach: String::new(),
        });
    }

    // Short instructions (≤6 words) without complexity markers → score 2
    let complexity_markers = [
        "and", "then", "compare", "analyze", "и", "потом", "затем",
        "сравни", "проанализируй",
    ];
    if word_count <= 6
        && !complexity_markers.iter().any(|m| {
            words.iter().any(|w| w.to_lowercase() == *m)
        })
    {
        return Some(Assessment {
            score: 2,
            reasoning: "short instruction".to_string(),
            suggested_approach: String::new(),
        });
    }

    None
}

pub async fn assess_complexity(
    provider: &Arc<dyn LlmProvider>,
    user_message: &str,
) -> Result<Assessment, WorkflowError> {
    let messages = vec![
        Message::System(ASSESSMENT_PROMPT.to_string()),
        Message::User(user_message.to_string()),
    ];

    let response = provider.chat(&messages, &[]).await?;
    let content = response
        .content
        .ok_or_else(|| WorkflowError::PlanningFailed("empty assessment response".into()))?;

    let json_str = extract_json(&content);

    serde_json::from_str::<Assessment>(json_str).map_err(|e| {
        WorkflowError::PlanningFailed(format!(
            "failed to parse assessment: {e}\nraw: {content}"
        ))
    })
}

/// Extract a JSON object from text that might contain markdown fences or prose.
pub(super) fn extract_json(s: &str) -> &str {
    let trimmed = s.trim();

    // ```json ... ```
    if let Some(start) = trimmed.find("```json") {
        let json_start = start + 7;
        if let Some(end) = trimmed[json_start..].find("```") {
            return trimmed[json_start..json_start + end].trim();
        }
    }

    // ``` ... ```
    if let Some(start) = trimmed.find("```") {
        let json_start = start + 3;
        if let Some(end) = trimmed[json_start..].find("```") {
            return trimmed[json_start..json_start + end].trim();
        }
    }

    // Bare { ... }
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            return &trimmed[start..=end];
        }
    }

    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_bare_json() {
        let input = r#"{"score": 5, "reasoning": "test"}"#;
        assert_eq!(extract_json(input), input);
    }

    #[test]
    fn extract_json_from_markdown() {
        let input = "Here is the result:\n```json\n{\"score\": 3}\n```\nDone.";
        assert_eq!(extract_json(input), "{\"score\": 3}");
    }

    #[test]
    fn extract_json_from_prose() {
        let input = "The complexity is {\"score\": 7, \"reasoning\": \"hard\"} as shown.";
        assert_eq!(
            extract_json(input),
            "{\"score\": 7, \"reasoning\": \"hard\"}"
        );
    }

    #[test]
    fn quick_assess_very_short() {
        let a = quick_assess("привет").unwrap();
        assert_eq!(a.score, 1);
        let a = quick_assess("hi there").unwrap();
        assert_eq!(a.score, 1);
        let a = quick_assess("do it").unwrap();
        assert_eq!(a.score, 1);
    }

    #[test]
    fn quick_assess_greeting() {
        let a = quick_assess("hello how are you doing today").unwrap();
        assert_eq!(a.score, 1);
        let a = quick_assess("привет как дела у тебя сегодня").unwrap();
        assert_eq!(a.score, 1);
        let a = quick_assess("добрый день подскажите пожалуйста").unwrap();
        assert_eq!(a.score, 1);
    }

    #[test]
    fn quick_assess_short_question() {
        let a = quick_assess("What time is it?").unwrap();
        assert_eq!(a.score, 2);
        let a = quick_assess("How does this work?").unwrap();
        assert_eq!(a.score, 2);
    }

    #[test]
    fn quick_assess_short_instruction() {
        let a = quick_assess("List all running processes").unwrap();
        assert_eq!(a.score, 2);
        let a = quick_assess("Show current disk usage stats").unwrap();
        assert_eq!(a.score, 2);
    }

    #[test]
    fn quick_assess_complexity_markers_block() {
        assert!(quick_assess("search and compare results").is_none());
        assert!(quick_assess("найди и потом сравни").is_none());
    }

    #[test]
    fn quick_assess_long_message_none() {
        assert!(quick_assess(
            "Research the best approaches for building a distributed system and write a report"
        ).is_none());
    }
}
