use async_openai::{
    config::OpenAIConfig,
    types::{AudioInput, CreateTranscriptionRequestArgs, InputSource},
    Client,
};

pub struct TranscriptionService {
    client: Client<OpenAIConfig>,
    model: String,
}

impl TranscriptionService {
    pub fn new(api_key: &str, base_url: Option<&str>) -> Self {
        let mut config = OpenAIConfig::default().with_api_key(api_key);
        if let Some(url) = base_url {
            config = config.with_api_base(url);
        }
        Self {
            client: Client::with_config(config),
            model: "whisper-1".to_string(),
        }
    }

    pub async fn transcribe(&self, audio_bytes: Vec<u8>, filename: &str) -> Result<String, String> {
        let request = CreateTranscriptionRequestArgs::default()
            .file(AudioInput {
                source: InputSource::VecU8 {
                    filename: filename.to_string(),
                    vec: audio_bytes,
                },
            })
            .model(&self.model)
            .build()
            .map_err(|e| format!("Failed to build transcription request: {e}"))?;

        let response = self
            .client
            .audio()
            .transcribe(request)
            .await
            .map_err(|e| format!("Transcription API error: {e}"))?;

        Ok(response.text)
    }
}
