pub mod login;
pub mod pkce;
pub mod storage;

pub const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_AUTH_AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
pub const OPENAI_AUTH_TOKEN: &str = "https://auth.openai.com/oauth/token";
pub const CHATGPT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";

pub const LOCAL_CALLBACK_PORT: u16 = 1455;
pub const LOCAL_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// How many days before we proactively refresh the token.
pub const TOKEN_REFRESH_INTERVAL_DAYS: i64 = 8;
