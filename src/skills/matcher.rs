use std::collections::HashSet;

use super::loader::SkillMeta;

/// Minimum score (0.0–1.0) for a skill to be considered relevant.
const MATCH_THRESHOLD: f64 = 0.25;
/// Maximum number of skills to inject per message.
const MAX_MATCHED_SKILLS: usize = 3;

/// Common English stop words filtered from both description and query.
const STOP_WORDS: &[&str] = &[
    "the", "this", "that", "with", "from", "will", "have", "has", "had",
    "been", "being", "should", "would", "could", "when", "where", "which",
    "what", "who", "how", "not", "are", "was", "were", "can", "does", "did",
    "its", "for", "and", "but", "they", "them", "their", "then", "than",
    "into", "also", "use", "used", "uses", "using", "user", "asks", "skill",
];

/// Tokenize text: lowercase, split on whitespace and punctuation, remove stop words.
fn tokenize(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|w| w.len() > 2)
        .filter(|w| !STOP_WORDS.contains(w))
        .map(String::from)
        .collect()
}

/// Score how well a skill description matches the user message.
/// Returns a value between 0.0 and 1.0.
fn score(skill: &SkillMeta, user_tokens: &HashSet<String>) -> f64 {
    let desc_tokens = tokenize(&skill.description);
    if desc_tokens.is_empty() {
        return 0.0;
    }
    let overlap = desc_tokens.intersection(user_tokens).count();
    overlap as f64 / desc_tokens.len() as f64
}

/// Find skills whose descriptions match the user message.
/// Returns up to `MAX_MATCHED_SKILLS` skills sorted by relevance.
pub fn match_skills<'a>(skills: &'a [SkillMeta], user_message: &str) -> Vec<&'a SkillMeta> {
    let user_tokens = tokenize(user_message);
    if user_tokens.is_empty() {
        return vec![];
    }

    let mut scored: Vec<(&SkillMeta, f64)> = skills
        .iter()
        .map(|s| (s, score(s, &user_tokens)))
        .filter(|(_, s)| *s >= MATCH_THRESHOLD)
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(MAX_MATCHED_SKILLS);

    scored.into_iter().map(|(s, _)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(name: &str, desc: &str) -> SkillMeta {
        SkillMeta {
            name: name.to_string(),
            description: desc.to_string(),
            version: None,
            body: "body".to_string(),
            base_dir: PathBuf::new(),
        }
    }

    #[test]
    fn tokenize_basic() {
        let tokens = tokenize("Hello, World! Run a test today.");
        assert!(tokens.contains("hello"));
        assert!(tokens.contains("world"));
        assert!(tokens.contains("run"));
        assert!(tokens.contains("test"));
        assert!(tokens.contains("today"));
        // "a" is <= 2 chars, should be filtered
        assert!(!tokens.contains("a"));
        // "the" is a stop word, should be filtered
        let tokens2 = tokenize("the quick fox");
        assert!(!tokens2.contains("the"));
        assert!(tokens2.contains("quick"));
        assert!(tokens2.contains("fox"));
    }

    #[test]
    fn match_relevant_skill() {
        let skills = vec![
            skill("web-research", "This skill should be used when the user asks to search the web or research topics online"),
            skill("code-review", "This skill should be used when the user asks to review code or find bugs"),
        ];

        let matched = match_skills(&skills, "please search the web for Rust tutorials");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "web-research");
    }

    #[test]
    fn no_match_below_threshold() {
        let skills = vec![
            skill("specific-tool", "This skill handles database migration for PostgreSQL schemas"),
        ];

        let matched = match_skills(&skills, "tell me a joke about cats");
        assert!(matched.is_empty());
    }

    #[test]
    fn multiple_matches_sorted() {
        let skills = vec![
            skill("skill-a", "publish articles and blog posts online"),
            skill("skill-b", "publish and deploy web applications online"),
            skill("skill-c", "something completely unrelated"),
        ];

        let matched = match_skills(&skills, "I want to publish my article online");
        assert!(matched.len() >= 1);
        // Both skill-a and skill-b should match, skill-c should not
        let names: Vec<&str> = matched.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"skill-c"));
    }

    #[test]
    fn empty_message_no_matches() {
        let skills = vec![skill("any", "some description here")];
        let matched = match_skills(&skills, "");
        assert!(matched.is_empty());
    }

    #[test]
    fn max_skills_limit() {
        let skills: Vec<SkillMeta> = (0..10)
            .map(|i| skill(&format!("skill-{i}"), "search web online research topics"))
            .collect();

        let matched = match_skills(&skills, "search web online research topics");
        assert!(matched.len() <= MAX_MATCHED_SKILLS);
    }

    #[test]
    fn score_empty_description() {
        let s = skill("empty", "");
        let tokens = tokenize("some query");
        assert_eq!(score(&s, &tokens), 0.0);
    }
}
