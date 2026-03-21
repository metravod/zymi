use std::path::Path;

use chrono::Utc;
use rand::Rng;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::pkce;
use super::storage::{save_tokens, AuthTokens};
use super::{
    CHATGPT_API_BASE, LOCAL_CALLBACK_PORT, LOCAL_REDIRECT_URI, OPENAI_AUTH_AUTHORIZE,
    OPENAI_AUTH_TOKEN, OPENAI_CLIENT_ID, TOKEN_REFRESH_INTERVAL_DAYS,
};

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
}

/// A model available via the ChatGPT backend.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatgptModel {
    pub slug: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub visibility: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    models: Vec<ChatgptModel>,
}

/// Run the full OAuth login flow with PKCE.
/// Opens the browser for authorization, waits for the callback, exchanges the code for tokens.
pub async fn login(memory_dir: &Path) -> anyhow::Result<()> {
    let pkce = pkce::generate();

    let state = {
        let mut bytes = [0u8; 16];
        rand::rng().fill(&mut bytes);
        hex::encode(bytes)
    };

    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        OPENAI_AUTH_AUTHORIZE,
        OPENAI_CLIENT_ID,
        urlencoding::encode(LOCAL_REDIRECT_URI),
        urlencoding::encode("openid profile email offline_access"),
        pkce.code_challenge,
        state,
    );

    println!();
    println!("  Opening browser for OpenAI authentication...");
    println!();
    println!("  If the browser doesn't open, visit:");
    println!("  {}", auth_url);
    println!();

    open_browser(&auth_url);

    // Start local server and wait for the callback
    let (code, returned_state) = wait_for_callback().await?;

    if returned_state != state {
        anyhow::bail!("OAuth state mismatch — possible CSRF attack");
    }

    println!("  Authorization received, exchanging code for tokens...");

    let tokens = exchange_code(&code, &pkce.code_verifier).await?;

    let auth = AuthTokens {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        last_refresh: Utc::now(),
    };

    save_tokens(memory_dir, &auth)?;

    println!();
    println!("  \x1b[1;32mLogin successful!\x1b[0m");

    // Fetch and configure available models
    println!("  Fetching available models...");
    match fetch_models(&auth.access_token).await {
        Ok(models) if !models.is_empty() => {
            let count = crate::setup::sync_chatgpt_models(memory_dir, &models);
            println!("  \x1b[1;32m+\x1b[0m {count} model(s) configured.");
        }
        Ok(_) => {
            println!("  \x1b[1;33m!\x1b[0m No models returned, using default (o4-mini).");
            crate::setup::ensure_chatgpt_model(memory_dir);
        }
        Err(e) => {
            log::warn!("Failed to fetch models: {e}");
            println!("  \x1b[1;33m!\x1b[0m Could not fetch models: {e}");
            println!("  Using default model (o4-mini).");
            crate::setup::ensure_chatgpt_model(memory_dir);
        }
    }

    println!();
    println!("  Run \x1b[1mzymi\x1b[0m to start chatting.");
    println!();

    Ok(())
}

/// Remote login flow for headless servers.
/// Prints the auth URL for the user to open on any device.
/// The user pastes back the full redirect URL (localhost will fail to load — that's fine,
/// we just need the URL from the browser address bar).
pub async fn login_remote(memory_dir: &Path) -> anyhow::Result<()> {
    let pkce = pkce::generate();

    let state = {
        let mut bytes = [0u8; 16];
        rand::rng().fill(&mut bytes);
        hex::encode(bytes)
    };

    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        OPENAI_AUTH_AUTHORIZE,
        OPENAI_CLIENT_ID,
        urlencoding::encode(LOCAL_REDIRECT_URI),
        urlencoding::encode("openid profile email offline_access"),
        pkce.code_challenge,
        state,
    );

    println!();
    println!("  \x1b[1mRemote login\x1b[0m — open this URL in any browser:");
    println!();
    println!("  {}", auth_url);
    println!();
    println!("  After authorizing, your browser will redirect to localhost (which will fail).");
    println!("  Copy the \x1b[1mfull URL\x1b[0m from the address bar and paste it here.");
    println!();
    print!("  Redirect URL: ");
    std::io::Write::flush(&mut std::io::stdout())?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.is_empty() {
        anyhow::bail!("No URL provided");
    }

    // Parse the pasted URL to extract code and state
    let query = input
        .split('?')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Invalid URL — no query parameters found"))?;

    let params: std::collections::HashMap<&str, &str> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect();

    if let Some(error) = params.get("error") {
        let desc = params.get("error_description").unwrap_or(&"");
        anyhow::bail!("OAuth error: {error} — {desc}");
    }

    let code = params
        .get("code")
        .ok_or_else(|| anyhow::anyhow!("No authorization code in URL"))?;
    let returned_state = params.get("state").unwrap_or(&"");

    if *returned_state != state {
        anyhow::bail!("OAuth state mismatch — possible CSRF attack");
    }

    println!("  Exchanging code for tokens...");

    let tokens = exchange_code(code, &pkce.code_verifier).await?;

    let auth = AuthTokens {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        last_refresh: Utc::now(),
    };

    save_tokens(memory_dir, &auth)?;

    println!();
    println!("  \x1b[1;32mLogin successful!\x1b[0m");

    // Fetch and configure available models
    println!("  Fetching available models...");
    match fetch_models(&auth.access_token).await {
        Ok(models) if !models.is_empty() => {
            let count = crate::setup::sync_chatgpt_models(memory_dir, &models);
            println!("  \x1b[1;32m+\x1b[0m {count} model(s) configured.");
        }
        Ok(_) => {
            println!("  \x1b[1;33m!\x1b[0m No models returned, using default (o4-mini).");
            crate::setup::ensure_chatgpt_model(memory_dir);
        }
        Err(e) => {
            log::warn!("Failed to fetch models: {e}");
            println!("  \x1b[1;33m!\x1b[0m Could not fetch models: {e}");
            println!("  Using default model (o4-mini).");
            crate::setup::ensure_chatgpt_model(memory_dir);
        }
    }

    println!();
    println!("  Run \x1b[1mzymi\x1b[0m to start chatting.");
    println!();

    Ok(())
}

/// Refresh the access token using the refresh token.
pub async fn refresh_token(memory_dir: &Path, auth: &AuthTokens) -> anyhow::Result<AuthTokens> {
    let client = reqwest::Client::new();

    let resp = client
        .post(OPENAI_AUTH_TOKEN)
        .json(&serde_json::json!({
            "client_id": OPENAI_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": auth.refresh_token,
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed ({status}): {body}");
    }

    let refresh_resp: RefreshResponse = resp.json().await?;

    let new_auth = AuthTokens {
        access_token: refresh_resp.access_token,
        refresh_token: refresh_resp.refresh_token,
        id_token: refresh_resp.id_token,
        last_refresh: Utc::now(),
    };

    save_tokens(memory_dir, &new_auth)?;
    log::info!("OAuth token refreshed successfully");

    Ok(new_auth)
}

/// Check if tokens need a proactive refresh (older than TOKEN_REFRESH_INTERVAL_DAYS).
pub fn needs_refresh(auth: &AuthTokens) -> bool {
    let age = Utc::now() - auth.last_refresh;
    age.num_days() >= TOKEN_REFRESH_INTERVAL_DAYS
}

/// Fetch available models from the ChatGPT backend.
/// Returns models sorted by priority (lower = higher priority), filtered to visible only.
pub async fn fetch_models(access_token: &str) -> anyhow::Result<Vec<ChatgptModel>> {
    let client = reqwest::Client::new();
    let version = env!("CARGO_PKG_VERSION");
    let url = format!("{}/models?client_version={}", CHATGPT_API_BASE, version);

    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to fetch models ({status}): {body}");
    }

    let models_resp: ModelsResponse = resp.json().await?;

    let mut models: Vec<ChatgptModel> = models_resp
        .models
        .into_iter()
        .filter(|m| m.visibility.as_deref() != Some("hide"))
        .collect();

    models.sort_by_key(|m| m.priority.unwrap_or(999));

    Ok(models)
}

/// Run the `zymi logout` flow.
pub fn logout(memory_dir: &Path) {
    super::storage::remove_tokens(memory_dir);
    println!();
    println!("  \x1b[1;32mLogged out.\x1b[0m Tokens removed.");
    println!();
}

// ============================================================
// Internal
// ============================================================

async fn wait_for_callback() -> anyhow::Result<(String, String)> {
    let listener = TcpListener::bind(format!("127.0.0.1:{LOCAL_CALLBACK_PORT}")).await?;
    println!("  Waiting for authorization callback on port {LOCAL_CALLBACK_PORT}...");

    let (mut stream, _) = listener.accept().await?;

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the GET request line for query parameters
    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");

    let query = path.split('?').nth(1).unwrap_or("");
    let params: std::collections::HashMap<&str, &str> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect();

    // Check for error before extracting code
    if let Some(error) = params.get("error") {
        let desc = params.get("error_description").unwrap_or(&"");
        let error_html = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
            <html><body><h2>Authentication failed</h2><p>{}: {}</p>\
            <p>You can close this tab.</p></body></html>",
            error, desc
        );
        let _ = stream.write_all(error_html.as_bytes()).await;
        let _ = stream.shutdown().await;
        anyhow::bail!("OAuth error: {error} — {desc}");
    }

    let code = params
        .get("code")
        .ok_or_else(|| anyhow::anyhow!("No authorization code in callback"))?
        .to_string();
    let state = params.get("state").unwrap_or(&"").to_string();

    // Send success response
    let success_html = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body><h2>Authentication successful!</h2>\
        <p>You can close this tab and return to Zymi.</p></body></html>";
    let _ = stream.write_all(success_html.as_bytes()).await;
    let _ = stream.shutdown().await;

    Ok((code, state))
}

async fn exchange_code(code: &str, code_verifier: &str) -> anyhow::Result<TokenResponse> {
    let client = reqwest::Client::new();

    let resp = client
        .post(OPENAI_AUTH_TOKEN)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            urlencoding::encode(code),
            urlencoding::encode(LOCAL_REDIRECT_URI),
            OPENAI_CLIENT_ID,
            urlencoding::encode(code_verifier),
        ))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed ({status}): {body}");
    }

    let token_resp: TokenResponse = resp.json().await?;
    Ok(token_resp)
}

fn open_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("xdg-open").arg(url).status()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()
    } else {
        return;
    };

    if let Err(e) = result {
        log::warn!("Failed to open browser: {e}");
    }
}
