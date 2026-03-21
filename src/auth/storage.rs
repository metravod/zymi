use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    pub last_refresh: DateTime<Utc>,
}

pub fn save_tokens(memory_dir: &Path, tokens: &AuthTokens) -> std::io::Result<()> {
    let path = memory_dir.join("auth.json");
    let content = serde_json::to_string_pretty(tokens)
        .map_err(std::io::Error::other)?;

    std::fs::write(&path, &content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

pub fn load_tokens(memory_dir: &Path) -> Option<AuthTokens> {
    let path = memory_dir.join("auth.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn remove_tokens(memory_dir: &Path) {
    let path = memory_dir.join("auth.json");
    let _ = std::fs::remove_file(path);
}
