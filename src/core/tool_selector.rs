use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::ToolDefinition;

const EMBEDDING_MODEL: &str = "text-embedding-3-small";
const EMBEDDING_DIMENSIONS: usize = 1536;
const DEFAULT_TOP_K: usize = 8;

#[derive(Debug, Serialize, Deserialize)]
struct CachedTool {
    hash: String,
    vector: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EmbeddingCache {
    model: String,
    tools: HashMap<String, CachedTool>,
}

struct ToolEmbedding {
    tool_name: String,
    vector: Vec<f32>,
}

pub struct ToolSelector {
    embeddings: Vec<ToolEmbedding>,
    always_available: HashSet<String>,
    top_k: usize,
    client: reqwest::Client,
    api_key: String,
    cache_path: Option<PathBuf>,
}

impl ToolSelector {
    pub fn new(
        api_key: String,
        cache_path: Option<PathBuf>,
        always_available: HashSet<String>,
    ) -> Self {
        Self {
            embeddings: Vec::new(),
            always_available,
            top_k: DEFAULT_TOP_K,
            client: reqwest::Client::new(),
            api_key,
            cache_path,
        }
    }

    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }

    /// Add multiple tools dynamically in a single batch embedding call.
    pub async fn add_tools(&mut self, tools: &[ToolDefinition]) -> Result<(), String> {
        let mut cache = self.load_cache();
        let mut to_embed: Vec<(String, String)> = Vec::new();

        for tool in tools {
            let text = format!("{}: {}", tool.name, tool.description);
            let hash = hash_text(&text);

            let needs_embed = if let Some(cached) = cache.tools.get(&tool.name) {
                cached.hash != hash
            } else {
                true
            };

            if needs_embed {
                // Remove stale embedding if exists
                self.embeddings.retain(|e| e.tool_name != tool.name);
                to_embed.push((tool.name.clone(), text));
            }
        }

        if to_embed.is_empty() {
            return Ok(());
        }

        log::info!("Dynamically embedding {} new tools", to_embed.len());
        let texts: Vec<String> = to_embed.iter().map(|(_, t)| t.clone()).collect();
        let vectors = self.embed_texts(&texts).await?;

        for ((name, text), vector) in to_embed.into_iter().zip(vectors.into_iter()) {
            let hash = hash_text(&text);
            cache.tools.insert(
                name.clone(),
                CachedTool {
                    hash,
                    vector: vector.clone(),
                },
            );
            self.embeddings.push(ToolEmbedding {
                tool_name: name,
                vector,
            });
        }

        self.save_cache(&cache);
        Ok(())
    }

    /// Compute or load embeddings for all tool definitions.
    pub async fn initialize(&mut self, tools: &[ToolDefinition]) -> Result<(), String> {
        let mut cache = self.load_cache();

        // Figure out which tools need embedding
        let mut to_embed: Vec<(String, String)> = Vec::new(); // (name, text)
        let mut up_to_date: Vec<ToolEmbedding> = Vec::new();

        for tool in tools {
            let text = format!("{}: {}", tool.name, tool.description);
            let hash = hash_text(&text);

            if let Some(cached) = cache.tools.get(&tool.name) {
                if cached.hash == hash && cached.vector.len() == EMBEDDING_DIMENSIONS {
                    up_to_date.push(ToolEmbedding {
                        tool_name: tool.name.clone(),
                        vector: cached.vector.clone(),
                    });
                    continue;
                }
            }
            to_embed.push((tool.name.clone(), text));
        }

        if !to_embed.is_empty() {
            log::info!(
                "Embedding {} tools ({} cached)",
                to_embed.len(),
                up_to_date.len()
            );
            let texts: Vec<String> = to_embed.iter().map(|(_, t)| t.clone()).collect();
            let vectors = self.embed_texts(&texts).await?;

            for ((name, text), vector) in to_embed.into_iter().zip(vectors.into_iter()) {
                let hash = hash_text(&text);
                cache.tools.insert(
                    name.clone(),
                    CachedTool {
                        hash,
                        vector: vector.clone(),
                    },
                );
                up_to_date.push(ToolEmbedding {
                    tool_name: name,
                    vector,
                });
            }

            // Remove stale entries
            let current_names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            cache.tools.retain(|k, _| current_names.contains(k.as_str()));

            self.save_cache(&cache);
        } else {
            log::info!("All {} tool embeddings loaded from cache", up_to_date.len());
        }

        self.embeddings = up_to_date;
        Ok(())
    }

    /// Select relevant tools for a query. Returns filtered + always-available definitions.
    pub async fn select_tools(
        &self,
        query: &str,
        all_definitions: &[ToolDefinition],
    ) -> Result<Vec<ToolDefinition>, String> {
        if self.embeddings.is_empty() {
            return Ok(all_definitions.to_vec());
        }

        let query_vector = self.embed_texts(&[query.to_string()]).await?;
        let query_vec = query_vector
            .into_iter()
            .next()
            .ok_or("Empty embedding response")?;

        // Score each tool
        let mut scores: Vec<(&str, f32)> = self
            .embeddings
            .iter()
            .map(|te| {
                let sim = cosine_similarity(&query_vec, &te.vector);
                (te.tool_name.as_str(), sim)
            })
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Build selected set: always-available + top-K
        let mut selected: HashSet<&str> = HashSet::new();
        for name in &self.always_available {
            selected.insert(name.as_str());
        }
        for (name, _score) in scores.iter().take(self.top_k) {
            selected.insert(name);
        }

        log::info!(
            "Tool selection: {} of {} tools (top-K={}, always={}). Top: [{}]",
            selected.len(),
            all_definitions.len(),
            self.top_k,
            self.always_available.len(),
            scores
                .iter()
                .take(5)
                .map(|(n, s)| format!("{n}={s:.3}"))
                .collect::<Vec<_>>()
                .join(", "),
        );

        Ok(all_definitions
            .iter()
            .filter(|d| selected.contains(d.name.as_str()))
            .cloned()
            .collect())
    }

    /// Call OpenAI embeddings API.
    async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        #[derive(Serialize)]
        struct EmbeddingRequest<'a> {
            model: &'a str,
            input: &'a [String],
        }

        #[derive(Deserialize)]
        struct EmbeddingResponse {
            data: Vec<EmbeddingData>,
        }

        #[derive(Deserialize)]
        struct EmbeddingData {
            embedding: Vec<f32>,
            index: usize,
        }

        let body = EmbeddingRequest {
            model: EMBEDDING_MODEL,
            input: texts,
        };

        let resp = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Embedding API request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Embedding API error {status}: {body}"));
        }

        let mut result: EmbeddingResponse = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse embedding response: {e}"))?;

        // Sort by index to match input order
        result.data.sort_by_key(|d| d.index);
        Ok(result.data.into_iter().map(|d| d.embedding).collect())
    }

    fn load_cache(&self) -> EmbeddingCache {
        let Some(ref path) = self.cache_path else {
            return empty_cache();
        };
        match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                log::warn!("Failed to parse embedding cache: {e}");
                empty_cache()
            }),
            Err(_) => empty_cache(),
        }
    }

    fn save_cache(&self, cache: &EmbeddingCache) {
        let Some(ref path) = self.cache_path else {
            return;
        };
        match serde_json::to_string_pretty(cache) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    log::warn!("Failed to write embedding cache: {e}");
                }
            }
            Err(e) => log::warn!("Failed to serialize embedding cache: {e}"),
        }
    }
}

fn empty_cache() -> EmbeddingCache {
    EmbeddingCache {
        model: EMBEDDING_MODEL.to_string(),
        tools: HashMap::new(),
    }
}

fn hash_text(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-10 {
        0.0
    } else {
        dot / denom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_vectors() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_orthogonal_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-5);
    }

    #[test]
    fn cosine_opposite_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-5);
    }

    #[test]
    fn hash_is_deterministic() {
        let h1 = hash_text("execute_shell: Run shell commands");
        let h2 = hash_text("execute_shell: Run shell commands");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_differs_for_different_text() {
        let h1 = hash_text("tool_a: does something");
        let h2 = hash_text("tool_b: does something else");
        assert_ne!(h1, h2);
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("embeddings.json");

        let selector = ToolSelector::new(
            "test-key".into(),
            Some(cache_path.clone()),
            HashSet::new(),
        );

        let mut cache = empty_cache();
        cache.tools.insert(
            "test_tool".into(),
            CachedTool {
                hash: "abc123".into(),
                vector: vec![0.1, 0.2, 0.3],
            },
        );

        selector.save_cache(&cache);

        let loaded = selector.load_cache();
        assert_eq!(loaded.tools.len(), 1);
        let tool = loaded.tools.get("test_tool").unwrap();
        assert_eq!(tool.hash, "abc123");
        assert_eq!(tool.vector.len(), 3);
    }

    #[test]
    fn empty_cache_on_missing_file() {
        let selector = ToolSelector::new(
            "test-key".into(),
            Some(PathBuf::from("/nonexistent/path/cache.json")),
            HashSet::new(),
        );
        let cache = selector.load_cache();
        assert!(cache.tools.is_empty());
    }
}
