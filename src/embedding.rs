//! Server-side embedding providers with bounded requests and write-only credentials.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/embeddings";
const VOYAGE_ENDPOINT: &str = "https://api.voyageai.com/v1/embeddings";
const MAX_INPUTS: usize = 256;
const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_TOTAL_TEXT_BYTES: usize = 1024 * 1024;
// Without a tokenizer, UTF-8 bytes conservatively bound OpenAI's byte-level
// token count. Keep one byte below the documented 8,192-token input limit.
// https://developers.openai.com/api/reference/resources/embeddings/methods/create
const OPENAI_MAX_TEXT_BYTES: usize = 8_191;
// Voyage prepends a query/document instruction. Reserve room for that prompt
// and tokenizer overhead; truncation:false remains the provider-side guard.
// https://docs.voyageai.com/docs/embeddings
const VOYAGE_PROMPT_BUDGET: usize = 128;
const VOYAGE_MAX_TEXT_BYTES: usize = 32_000 - VOYAGE_PROMPT_BUDGET;
static CONFIG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    Openai,
    Voyage,
}

impl Provider {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Voyage => "voyage",
        }
    }

    fn key_index(self) -> usize {
        match self {
            Self::Openai => 0,
            Self::Voyage => 1,
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            Self::Openai => "text-embedding-3-small",
            Self::Voyage => "voyage-4",
        }
    }

    fn models(self) -> Vec<ModelInfo> {
        match self {
            Self::Openai => vec![
                ModelInfo::new("text-embedding-3-small", &[1536], 1536),
                ModelInfo::new("text-embedding-3-large", &[3072], 3072),
            ],
            Self::Voyage => ["voyage-4", "voyage-4-large", "voyage-4-lite"]
                .into_iter()
                .map(|id| ModelInfo::new(id, &[256, 512, 1024, 2048], 1024))
                .collect(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    provider: Provider,
    model: String,
    dimensions: usize,
    batch_size: usize,
    timeout_seconds: u64,
    max_concurrent_requests: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: Provider::Openai,
            model: "text-embedding-3-small".into(),
            dimensions: 1536,
            batch_size: 32,
            timeout_seconds: 60,
            max_concurrent_requests: 4,
        }
    }
}

impl Config {
    fn validate(&self) -> Result<(), EmbeddingError> {
        let model = self
            .provider
            .models()
            .into_iter()
            .find(|model| model.id == self.model)
            .ok_or(EmbeddingError::Invalid(
                "unsupported embedding model for this provider",
            ))?;
        let valid_dimensions = match self.provider {
            Provider::Openai => self.dimensions > 0 && self.dimensions <= model.default_dimensions,
            Provider::Voyage => model.dimensions.contains(&self.dimensions),
        };
        if !valid_dimensions {
            return Err(EmbeddingError::Invalid(
                "unsupported dimensions for this embedding model",
            ));
        }
        if !(1..=128).contains(&self.batch_size) {
            return Err(EmbeddingError::Invalid(
                "batch_size must be between 1 and 128",
            ));
        }
        if !(1..=120).contains(&self.timeout_seconds) {
            return Err(EmbeddingError::Invalid(
                "timeout_seconds must be between 1 and 120",
            ));
        }
        if !(1..=16).contains(&self.max_concurrent_requests) {
            return Err(EmbeddingError::Invalid(
                "max_concurrent_requests must be between 1 and 16",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub(crate) struct ModelInfo {
    id: &'static str,
    dimensions: &'static [usize],
    default_dimensions: usize,
}

impl ModelInfo {
    fn new(id: &'static str, dimensions: &'static [usize], default_dimensions: usize) -> Self {
        Self {
            id,
            dimensions,
            default_dimensions,
        }
    }
}

#[derive(Serialize)]
struct ProviderInfo {
    id: Provider,
    label: &'static str,
    configured: bool,
    models: Vec<ModelInfo>,
}

#[derive(Serialize)]
pub(crate) struct SettingsResponse {
    #[serde(flatten)]
    config: Config,
    configured: bool,
    providers: Vec<ProviderInfo>,
    persistence: &'static str,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsUpdate {
    provider: Option<Provider>,
    model: Option<String>,
    dimensions: Option<usize>,
    batch_size: Option<usize>,
    timeout_seconds: Option<u64>,
    max_concurrent_requests: Option<usize>,
    api_key: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InputType {
    Query,
    #[default]
    Document,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerateRequest {
    input: Vec<String>,
    #[serde(default)]
    input_type: InputType,
    expected_settings: Option<ExpectedSettings>,
}

impl GenerateRequest {
    /// Use the collection's stored profile for every document or query call.
    /// A differing active profile is rejected before contacting a provider.
    pub(crate) fn pinned(
        input: Vec<String>,
        input_type: InputType,
        expected_settings: ExpectedSettings,
    ) -> Self {
        Self {
            input,
            input_type,
            expected_settings: Some(expected_settings),
        }
    }
}

/// Non-secret identity of a vector space. Store this with the collection and
/// reuse it for both document chunks and queries; dimensions alone are not an
/// embedding compatibility check.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedSettings {
    pub(crate) provider: Provider,
    pub(crate) model: String,
    pub(crate) dimensions: usize,
}

impl ExpectedSettings {
    pub(crate) fn new(
        provider: &str,
        model: &str,
        dimensions: usize,
    ) -> Result<Self, EmbeddingError> {
        let provider = match provider {
            "openai" => Provider::Openai,
            "voyage" => Provider::Voyage,
            _ => return Err(EmbeddingError::Invalid("unsupported embedding provider")),
        };
        let config = Config {
            provider,
            model: model.into(),
            dimensions,
            ..Config::default()
        };
        config.validate()?;
        Ok(Self::from_config(&config))
    }

    fn from_config(config: &Config) -> Self {
        Self {
            provider: config.provider,
            model: config.model.clone(),
            dimensions: config.dimensions,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct GenerateResponse {
    pub(crate) provider: Provider,
    pub(crate) model: String,
    pub(crate) dimensions: usize,
    /// The requested retrieval role. OpenAI has no input_type parameter; Voyage
    /// receives this role explicitly, with truncation disabled.
    pub(crate) input_type: InputType,
    pub(crate) embeddings: Vec<Vec<f32>>,
    pub(crate) usage: Usage,
}

impl GenerateResponse {
    pub(crate) fn profile(&self) -> ExpectedSettings {
        ExpectedSettings {
            provider: self.provider,
            model: self.model.clone(),
            dimensions: self.dimensions,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) total_tokens: u64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EmbeddingError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("configure an API key for the selected embedding provider")]
    NotConfigured,
    #[error("embedding settings changed; reload the settings before generating vectors")]
    SettingsChanged,
    #[error("embedding request capacity is exhausted; retry later")]
    Busy,
    #[error("embedding provider rejected the API key; update the provider credentials")]
    Authentication,
    #[error("embedding provider rate limit reached; retry later")]
    RateLimited,
    #[error("embedding provider rejected the input; check text length and model settings")]
    Rejected,
    #[error("embedding provider request timed out; it may have been processed, so check usage before retrying")]
    Timeout,
    #[error("embedding provider is unavailable; the request was not retried")]
    Unavailable,
    #[error("embedding provider returned an invalid or oversized response")]
    InvalidResponse,
    #[error("could not save embedding settings")]
    Persistence,
    #[error("embedding service state is unavailable")]
    Internal,
}

struct RuntimeState {
    config: Config,
    keys: [Option<String>; 2],
}

struct Inner {
    client: reqwest::Client,
    state: RwLock<RuntimeState>,
    updates: Mutex<()>,
    in_flight: AtomicUsize,
    settings_in_flight: AtomicUsize,
    config_path: Option<PathBuf>,
    endpoints: [String; 2],
}

/// Shared provider client and embedding settings. Credentials never enter persisted settings.
#[derive(Clone)]
pub struct EmbeddingService {
    inner: Arc<Inner>,
}

impl EmbeddingService {
    /// Load non-secret settings and API keys from OPENAI_API_KEY / VOYAGE_API_KEY.
    /// A supplied path stores future non-secret settings updates atomically.
    pub fn from_environment(config_path: Option<PathBuf>) -> io::Result<Self> {
        let config = match config_path.as_ref().map(fs::read) {
            Some(Ok(bytes)) => serde_json::from_slice::<Config>(&bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid embedding settings file",
                )
            })?,
            Some(Err(error)) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => Config::default(),
        };
        config.validate().map_err(io::Error::other)?;
        let keys = ["OPENAI_API_KEY", "VOYAGE_API_KEY"].map(|name| {
            std::env::var(name)
                .ok()
                .map(|key| key.trim().to_owned())
                .filter(|key| !key.is_empty())
        });
        for key in keys.iter().flatten() {
            validate_key(key).map_err(io::Error::other)?;
        }
        Self::build(
            config,
            keys,
            config_path,
            [OPENAI_ENDPOINT.into(), VOYAGE_ENDPOINT.into()],
            true,
        )
    }

    fn build(
        config: Config,
        keys: [Option<String>; 2],
        config_path: Option<PathBuf>,
        endpoints: [String; 2],
        https_only: bool,
    ) -> io::Result<Self> {
        let client = reqwest::Client::builder()
            .https_only(https_only)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(16)
            .build()
            .map_err(|_| io::Error::other("could not initialize embedding HTTP client"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                state: RwLock::new(RuntimeState { config, keys }),
                updates: Mutex::new(()),
                in_flight: AtomicUsize::new(0),
                settings_in_flight: AtomicUsize::new(0),
                config_path,
                endpoints,
            }),
        })
    }

    pub(crate) fn settings(&self) -> Result<SettingsResponse, EmbeddingError> {
        let state = self
            .inner
            .state
            .read()
            .map_err(|_| EmbeddingError::Internal)?;
        Ok(settings_response(&state, self.inner.config_path.is_some()))
    }

    /// Capture the active non-secret vector-space identity. Generation must
    /// still use GenerateRequest::pinned because settings can change afterward.
    pub(crate) fn profile(&self) -> Result<ExpectedSettings, EmbeddingError> {
        let state = self
            .inner
            .state
            .read()
            .map_err(|_| EmbeddingError::Internal)?;
        Ok(ExpectedSettings::from_config(&state.config))
    }

    pub(crate) fn acquire_settings_update(&self) -> Result<SettingsUpdatePermit, EmbeddingError> {
        self.inner
            .settings_in_flight
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| EmbeddingError::Busy)?;
        Ok(SettingsUpdatePermit(self.inner.clone()))
    }

    // Run on the caller's blocking pool. The whole update, including publication
    // to readers, completes even if the requesting HTTP client disconnects.
    pub(crate) fn update_settings(
        &self,
        update: SettingsUpdate,
    ) -> Result<SettingsResponse, EmbeddingError> {
        let _update = self
            .inner
            .updates
            .lock()
            .map_err(|_| EmbeddingError::Internal)?;
        let mut config = self
            .inner
            .state
            .read()
            .map_err(|_| EmbeddingError::Internal)?
            .config
            .clone();
        let old_provider = config.provider;
        let old_model = config.model.clone();
        if let Some(provider) = update.provider {
            config.provider = provider;
        }
        if let Some(model) = update.model {
            config.model = model;
        } else if config.provider != old_provider {
            config.model = config.provider.default_model().into();
        }
        if let Some(dimensions) = update.dimensions {
            config.dimensions = dimensions;
        } else if config.provider != old_provider || config.model != old_model {
            config.dimensions = config
                .provider
                .models()
                .into_iter()
                .find(|model| model.id == config.model)
                .ok_or(EmbeddingError::Invalid(
                    "unsupported embedding model for this provider",
                ))?
                .default_dimensions;
        }
        if let Some(value) = update.batch_size {
            config.batch_size = value;
        }
        if let Some(value) = update.timeout_seconds {
            config.timeout_seconds = value;
        }
        if let Some(value) = update.max_concurrent_requests {
            config.max_concurrent_requests = value;
        }
        config.validate()?;
        if update.api_key.is_some() && update.clear_api_key {
            return Err(EmbeddingError::Invalid(
                "provide api_key or clear_api_key, not both",
            ));
        }
        let key = update.api_key.map(|key| key.trim().to_owned());
        if let Some(key) = &key {
            validate_key(key)?;
        }
        if let Some(path) = &self.inner.config_path {
            persist_config(path, &config)?;
        }
        let mut state = self
            .inner
            .state
            .write()
            .map_err(|_| EmbeddingError::Internal)?;
        let key_index = config.provider.key_index();
        if let Some(key) = key {
            state.keys[key_index] = Some(key);
        } else if update.clear_api_key {
            state.keys[key_index] = None;
        }
        state.config = config;
        Ok(settings_response(&state, self.inner.config_path.is_some()))
    }

    pub(crate) async fn generate(
        &self,
        request: GenerateRequest,
    ) -> Result<GenerateResponse, EmbeddingError> {
        if request.input.is_empty() || request.input.len() > MAX_INPUTS {
            return Err(EmbeddingError::Invalid(
                "input must contain between 1 and 256 texts",
            ));
        }
        if request
            .input
            .iter()
            .any(|text| text.trim().is_empty() || text.len() > MAX_TEXT_BYTES)
        {
            return Err(EmbeddingError::Invalid(
                "each input must contain nonempty text of at most 32768 bytes",
            ));
        }
        if request.input.iter().map(String::len).sum::<usize>() > MAX_TOTAL_TEXT_BYTES {
            return Err(EmbeddingError::Invalid(
                "combined input text exceeds 1048576 bytes",
            ));
        }
        let (config, key) = {
            let state = self
                .inner
                .state
                .read()
                .map_err(|_| EmbeddingError::Internal)?;
            (
                state.config.clone(),
                state.keys[state.config.provider.key_index()].clone(),
            )
        };
        if request.expected_settings.as_ref().is_some_and(|expected| {
            expected.provider != config.provider
                || expected.model != config.model
                || expected.dimensions != config.dimensions
        }) {
            return Err(EmbeddingError::SettingsChanged);
        }
        validate_provider_input(&config, &request.input)?;
        let key = key.ok_or(EmbeddingError::NotConfigured)?;
        self.inner
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < config.max_concurrent_requests).then_some(current + 1)
            })
            .map_err(|_| EmbeddingError::Busy)?;
        let _permit = EmbeddingPermit(self.inner.clone());
        tokio::time::timeout(
            Duration::from_secs(config.timeout_seconds),
            self.generate_batches(&config, &key, request),
        )
        .await
        .map_err(|_| EmbeddingError::Timeout)?
    }

    async fn generate_batches(
        &self,
        config: &Config,
        key: &str,
        request: GenerateRequest,
    ) -> Result<GenerateResponse, EmbeddingError> {
        let mut embeddings = Vec::with_capacity(request.input.len());
        let mut total_tokens = 0u64;
        let mut start = 0;
        while start < request.input.len() {
            let count = provider_batch_size(config, &request.input[start..]);
            if count == 0 {
                return Err(EmbeddingError::Invalid(
                    "an input exceeds the embedding provider batch budget",
                ));
            }
            let input = &request.input[start..start + count];
            let body = match config.provider {
                Provider::Openai => {
                    json!({ "model": config.model, "input": input, "dimensions": config.dimensions, "encoding_format": "float" })
                }
                Provider::Voyage => {
                    json!({ "model": config.model, "input": input, "output_dimension": config.dimensions, "output_dtype": "float", "input_type": request.input_type, "truncation": false })
                }
            };
            let mut response = self
                .inner
                .client
                .post(&self.inner.endpoints[config.provider.key_index()])
                .bearer_auth(key)
                .json(&body)
                .send()
                .await
                .map_err(provider_transport_error)?;
            if !response.status().is_success() {
                return Err(match response.status().as_u16() {
                    401 | 403 => EmbeddingError::Authentication,
                    429 => EmbeddingError::RateLimited,
                    400 | 413 | 422 => EmbeddingError::Rejected,
                    _ => EmbeddingError::Unavailable,
                });
            }
            let max_bytes = input.len() * config.dimensions * 32 + 8192;
            if response
                .content_length()
                .is_some_and(|length| length > max_bytes as u64)
            {
                return Err(EmbeddingError::InvalidResponse);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(provider_transport_error)? {
                if chunk.len() > max_bytes.saturating_sub(bytes.len()) {
                    return Err(EmbeddingError::InvalidResponse);
                }
                bytes.extend_from_slice(&chunk);
            }
            let batch: ProviderResponse =
                serde_json::from_slice(&bytes).map_err(|_| EmbeddingError::InvalidResponse)?;
            total_tokens = total_tokens
                .checked_add(batch.usage.total_tokens)
                .ok_or(EmbeddingError::InvalidResponse)?;
            embeddings.extend(validate_embeddings(
                batch.data,
                input.len(),
                config.dimensions,
            )?);
            start += count;
        }
        Ok(GenerateResponse {
            provider: config.provider,
            model: config.model.clone(),
            dimensions: config.dimensions,
            input_type: request.input_type,
            embeddings,
            usage: Usage { total_tokens },
        })
    }
}

fn validate_provider_input(config: &Config, input: &[String]) -> Result<(), EmbeddingError> {
    let (maximum, message) = match config.provider {
        Provider::Openai => (
            OPENAI_MAX_TEXT_BYTES,
            "OpenAI input must not exceed 8191 UTF-8 bytes; split longer text into chunks",
        ),
        Provider::Voyage => (
            VOYAGE_MAX_TEXT_BYTES,
            "Voyage input must not exceed 31872 UTF-8 bytes; split longer text into chunks",
        ),
    };
    if input.iter().any(|text| text.len() > maximum) {
        return Err(EmbeddingError::Invalid(message));
    }
    Ok(())
}

/// Keep both the configured item count and a conservative provider token
/// budget. These are UTF-8 byte estimates, not measured tokenizer usage.
fn provider_batch_size(config: &Config, input: &[String]) -> usize {
    let (maximum, prompt_budget) = match config.provider {
        Provider::Openai => (300_000, 0),
        Provider::Voyage => (
            match config.model.as_str() {
                "voyage-4-lite" => 1_000_000,
                "voyage-4" => 320_000,
                _ => 120_000,
            },
            VOYAGE_PROMPT_BUDGET,
        ),
    };
    let mut remaining = maximum;
    input
        .iter()
        .take(config.batch_size)
        .take_while(|text| {
            let cost = text.len().saturating_add(prompt_budget);
            if cost > remaining {
                return false;
            }
            remaining -= cost;
            true
        })
        .count()
}

struct EmbeddingPermit(Arc<Inner>);
impl Drop for EmbeddingPermit {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Release);
    }
}

pub(crate) struct SettingsUpdatePermit(Arc<Inner>);
impl Drop for SettingsUpdatePermit {
    fn drop(&mut self) {
        self.0.settings_in_flight.store(0, Ordering::Release);
    }
}

fn provider_transport_error(error: reqwest::Error) -> EmbeddingError {
    if error.is_timeout() {
        EmbeddingError::Timeout
    } else {
        EmbeddingError::Unavailable
    }
}

fn validate_key(key: &str) -> Result<(), EmbeddingError> {
    if key.is_empty() || key.len() > 4096 || reqwest::header::HeaderValue::from_str(key).is_err() {
        return Err(EmbeddingError::Invalid(
            "api_key must be a nonempty valid HTTP credential of at most 4096 bytes",
        ));
    }
    Ok(())
}

fn settings_response(state: &RuntimeState, durable: bool) -> SettingsResponse {
    SettingsResponse {
        config: state.config.clone(),
        configured: state.keys[state.config.provider.key_index()].is_some(),
        providers: [
            (Provider::Openai, "OpenAI"),
            (Provider::Voyage, "Voyage AI"),
        ]
        .into_iter()
        .map(|(id, label)| ProviderInfo {
            id,
            label,
            configured: state.keys[id.key_index()].is_some(),
            models: id.models(),
        })
        .collect(),
        persistence: if durable { "durable" } else { "memory" },
    }
}

fn persist_config(path: &Path, config: &Config) -> Result<(), EmbeddingError> {
    let parent = path.parent().ok_or(EmbeddingError::Persistence)?;
    let sequence = CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".embeddings-{}-{sequence}.tmp", std::process::id()));
    let result = (|| -> io::Result<()> {
        fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(config).map_err(io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|_| EmbeddingError::Persistence)
}

#[derive(Deserialize)]
struct ProviderResponse {
    data: Vec<ProviderEmbedding>,
    #[serde(default)]
    usage: Usage,
}
#[derive(Deserialize)]
struct ProviderEmbedding {
    index: usize,
    embedding: Vec<f32>,
}
fn validate_embeddings(
    data: Vec<ProviderEmbedding>,
    count: usize,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>, EmbeddingError> {
    if data.len() != count {
        return Err(EmbeddingError::InvalidResponse);
    }
    let mut ordered = vec![None; count];
    for item in data {
        if item.index >= count
            || ordered[item.index].is_some()
            || item.embedding.len() != dimensions
            || item.embedding.iter().any(|value| !value.is_finite())
        {
            return Err(EmbeddingError::InvalidResponse);
        }
        ordered[item.index] = Some(item.embedding);
    }
    ordered
        .into_iter()
        .map(|value| value.ok_or(EmbeddingError::InvalidResponse))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    pub(crate) fn test_service(path: Option<PathBuf>) -> EmbeddingService {
        EmbeddingService::build(
            Config::default(),
            [None, None],
            path,
            ["http://127.0.0.1:1".into(), "http://127.0.0.1:1".into()],
            false,
        )
        .unwrap()
    }

    fn request(input: &[&str]) -> GenerateRequest {
        GenerateRequest {
            input: input.iter().map(|text| (*text).into()).collect(),
            input_type: InputType::Query,
            expected_settings: None,
        }
    }

    pub(crate) fn configured(endpoint: &str, provider: Provider) -> EmbeddingService {
        let config = Config {
            provider,
            model: provider.default_model().into(),
            dimensions: if provider == Provider::Openai { 2 } else { 256 },
            batch_size: 2,
            max_concurrent_requests: 1,
            ..Config::default()
        };
        EmbeddingService::build(
            config,
            [
                Some("synthetic-openai-key".into()),
                Some("synthetic-voyage-key".into()),
            ],
            None,
            [endpoint.into(), endpoint.into()],
            false,
        )
        .unwrap()
    }

    pub(crate) fn mock_provider(
        count: usize,
        respond: impl Fn(usize, serde_json::Value, &str) -> (u16, serde_json::Value) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        mock_provider_encoded(count, false, respond)
    }

    fn mock_provider_encoded(
        count: usize,
        chunked: bool,
        respond: impl Fn(usize, serde_json::Value, &str) -> (u16, serde_json::Value) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/embeddings", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            for index in 0..count {
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "mock provider timed out waiting for request {} of {count}",
                                index + 1
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("mock provider accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.is_empty());
                    headers.push_str(&line);
                }
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                let body = serde_json::from_slice(&bytes).unwrap();
                let (status, response) = respond(index, body, &headers);
                let bytes = serde_json::to_vec(&response).unwrap();
                let mut stream = reader.into_inner();
                if chunked {
                    let _ = write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                    for chunk in bytes.chunks(512) {
                        let _ = write!(stream, "{:x}\r\n", chunk.len());
                        let _ = stream.write_all(chunk);
                        let _ = stream.write_all(b"\r\n");
                    }
                    let _ = stream.write_all(b"0\r\n\r\n");
                } else {
                    let _ = write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
                    let _ = stream.write_all(&bytes);
                }
            }
        });
        (endpoint, handle)
    }

    #[actix_web::test]
    async fn batches_openai_and_reorders_provider_indices() {
        let (endpoint, mock) = mock_provider(2, |batch, body, headers| {
            assert!(headers.contains("Bearer synthetic-openai-key"));
            assert_eq!(body["model"], "text-embedding-3-small");
            assert_eq!(body["encoding_format"], "float");
            assert_eq!(body["dimensions"], 2);
            assert!(body.get("input_type").is_none());
            let input = body["input"].as_array().unwrap();
            assert_eq!(input.len(), if batch == 0 { 2 } else { 1 });
            let data = input.iter().enumerate().rev().map(|(index, text)| {
                json!({ "index": index, "embedding": [text.as_str().unwrap().len() as f32, 0.5] })
            }).collect::<Vec<_>>();
            (
                200,
                json!({ "data": data, "usage": { "total_tokens": input.len() } }),
            )
        });
        let service = configured(&endpoint, Provider::Openai);
        let response = service
            .generate(request(&["a", "bb", "ccc"]))
            .await
            .unwrap();
        assert_eq!(
            response.embeddings,
            vec![vec![1.0, 0.5], vec![2.0, 0.5], vec![3.0, 0.5]]
        );
        assert_eq!(response.usage.total_tokens, 3);
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn voyage_uses_explicit_dimensions_dtype_and_input_type() {
        let (endpoint, mock) = mock_provider(1, |_, body, headers| {
            assert!(headers.contains("Bearer synthetic-voyage-key"));
            assert_eq!(body["model"], "voyage-4");
            assert_eq!(body["output_dimension"], 256);
            assert_eq!(body["output_dtype"], "float");
            assert_eq!(body["input_type"], "document");
            assert_eq!(body["truncation"], false);
            (
                200,
                json!({ "data": [{ "index": 0, "embedding": vec![0.25; 256] }], "usage": { "total_tokens": 7 } }),
            )
        });
        let service = configured(&endpoint, Provider::Voyage);
        let mut input = request(&["document"]);
        input.input_type = InputType::Document;
        let response = service.generate(input).await.unwrap();
        assert_eq!(response.embeddings[0].len(), 256);
        mock.join().unwrap();
    }

    #[test]
    fn collection_profiles_are_validated_serializable_and_non_secret() {
        let service = configured("http://127.0.0.1:1", Provider::Openai);
        let profile = service.profile().unwrap();
        assert_eq!(profile.provider.as_str(), "openai");
        assert_eq!(
            profile,
            ExpectedSettings::new("openai", "text-embedding-3-small", 2).unwrap()
        );
        let encoded = serde_json::to_value(&profile).unwrap();
        assert_eq!(
            encoded,
            json!({"provider":"openai", "model":"text-embedding-3-small", "dimensions":2})
        );
        assert_eq!(
            serde_json::from_value::<ExpectedSettings>(encoded).unwrap(),
            profile
        );
        for (provider, model, dimensions) in [
            ("unsupported", "text-embedding-3-small", 2),
            ("openai", "voyage-4", 256),
            ("openai", "text-embedding-3-small", 0),
            ("openai", "text-embedding-3-small", 1537),
            ("voyage", "voyage-4-large", 2),
        ] {
            assert!(ExpectedSettings::new(provider, model, dimensions).is_err());
        }
        assert_eq!(
            ExpectedSettings::new("voyage", "voyage-4-large", 256)
                .unwrap()
                .provider
                .as_str(),
            "voyage"
        );
    }

    #[actix_web::test]
    async fn validates_entire_input_byte_budget_before_any_provider_call() {
        let service = configured("http://127.0.0.1:1", Provider::Openai);
        // UTF-8 bytes include title/heading prefixes and multi-byte characters.
        let accepted = format!("Title\n{}", "雪".repeat(2728));
        assert_eq!(accepted.len(), 8190);
        assert!(
            validate_provider_input(&Config::default(), std::slice::from_ref(&accepted)).is_ok()
        );
        let rejected = format!("{accepted}雪");
        assert!(matches!(
            service.generate(GenerateRequest::pinned(
                vec!["valid first chunk".into(), rejected],
                InputType::Document,
                service.profile().unwrap(),
            )).await,
            Err(EmbeddingError::Invalid(message)) if message.contains("8191")
        ));
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        for (input, message) in [
            (vec!["chunk".into(); MAX_INPUTS + 1], "256"),
            (vec!["x".repeat(4097); MAX_INPUTS], "1048576"),
        ] {
            assert!(matches!(
                service.generate(GenerateRequest::pinned(input, InputType::Document, service.profile().unwrap())).await,
                Err(EmbeddingError::Invalid(actual)) if actual.contains(message)
            ));
        }
        let voyage = Config {
            provider: Provider::Voyage,
            model: "voyage-4".into(),
            ..Config::default()
        };
        assert!(validate_provider_input(&voyage, &["x".repeat(VOYAGE_MAX_TEXT_BYTES)]).is_ok());
        assert!(
            validate_provider_input(&voyage, &["x".repeat(VOYAGE_MAX_TEXT_BYTES + 1)]).is_err()
        );
    }

    #[test]
    fn provider_batches_bound_items_and_each_model_total_input_budget() {
        let input = vec!["x".repeat(4096); 256];
        for (provider, model, expected) in [
            (Provider::Openai, "text-embedding-3-small", 73),
            (Provider::Voyage, "voyage-4-large", 28),
            (Provider::Voyage, "voyage-4", 75),
            (Provider::Voyage, "voyage-4-lite", 128),
        ] {
            let mut config = Config {
                provider,
                model: model.into(),
                batch_size: 128,
                ..Config::default()
            };
            assert_eq!(provider_batch_size(&config, &input), expected, "{model}");
            config.batch_size = 7;
            assert_eq!(provider_batch_size(&config, &input), 7, "{model}");
            assert_eq!(provider_batch_size(&config, &[]), 0);
        }
    }

    #[actix_web::test]
    async fn full_chunk_request_is_split_by_provider_budget_and_keeps_provenance() {
        let (endpoint, mock) = mock_provider(4, |batch, body, _| {
            assert_eq!(body["model"], "text-embedding-3-small");
            assert!(body.get("input_type").is_none());
            let input = body["input"].as_array().unwrap();
            assert_eq!(input.len(), [73, 73, 73, 37][batch]);
            assert!(
                input
                    .iter()
                    .map(|text| text.as_str().unwrap().len())
                    .sum::<usize>()
                    <= 300_000
            );
            let data = input
                .iter()
                .enumerate()
                .rev()
                .map(|(index, text)| {
                    let id = text.as_str().unwrap()[..3].parse::<f32>().unwrap();
                    json!({"index":index, "embedding":[id, 0.5]})
                })
                .collect::<Vec<_>>();
            (
                200,
                json!({"data":data,"usage":{"total_tokens":input.len()}}),
            )
        });
        let service = configured(&endpoint, Provider::Openai);
        service
            .update_settings(SettingsUpdate {
                batch_size: Some(128),
                ..SettingsUpdate::default()
            })
            .unwrap();
        let profile = service.profile().unwrap();
        let input = (0..256)
            .map(|index| format!("{index:03}{}", "x".repeat(4093)))
            .collect();
        let response = service
            .generate(GenerateRequest::pinned(
                input,
                InputType::Document,
                profile.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.profile(), profile);
        assert_eq!(response.input_type, InputType::Document);
        assert_eq!(response.embeddings.len(), 256);
        assert_eq!(response.usage.total_tokens, 256);
        for (index, embedding) in response.embeddings.iter().enumerate() {
            assert_eq!(embedding, &[index as f32, 0.5]);
        }
        assert_eq!(
            serde_json::to_value(&response).unwrap()["input_type"],
            "document"
        );
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn pinned_voyage_document_and_query_use_distinct_retrieval_roles() {
        let (endpoint, mock) = mock_provider(2, |index, body, _| {
            assert_eq!(body["input_type"], ["document", "query"][index]);
            assert_eq!(body["truncation"], false);
            assert_eq!(body["model"], "voyage-4");
            (
                200,
                json!({"data":[{"index":0,"embedding":vec![0.25; 256]}]}),
            )
        });
        let service = configured(&endpoint, Provider::Voyage);
        let profile = service.profile().unwrap();
        for input_type in [InputType::Document, InputType::Query] {
            let response = service
                .generate(GenerateRequest::pinned(
                    vec!["text".into()],
                    input_type,
                    profile.clone(),
                ))
                .await
                .unwrap();
            assert_eq!(response.profile(), profile);
            assert_eq!(response.input_type, input_type);
        }
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn settings_change_cannot_mix_inflight_batches_or_later_collection_queries() {
        let shared_service = Arc::new(Mutex::new(None::<EmbeddingService>));
        let provider_service = shared_service.clone();
        let (endpoint, mock) = mock_provider(2, move |batch, body, _| {
            assert_eq!(body["model"], "text-embedding-3-small");
            assert_eq!(body["dimensions"], 2);
            if batch == 0 {
                provider_service
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .update_settings(SettingsUpdate {
                        model: Some("text-embedding-3-large".into()),
                        dimensions: Some(4),
                        ..SettingsUpdate::default()
                    })
                    .unwrap();
            }
            let data = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, _)| json!({"index":index,"embedding":[0.25,0.5]}))
                .collect::<Vec<_>>();
            (200, json!({"data":data}))
        });
        let service = configured(&endpoint, Provider::Openai);
        *shared_service.lock().unwrap() = Some(service.clone());
        let original = service.profile().unwrap();
        let response = service
            .generate(GenerateRequest::pinned(
                vec!["first".into(), "second".into(), "third".into()],
                InputType::Document,
                original.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.profile(), original);
        assert_eq!(response.embeddings.len(), 3);
        assert_eq!(service.profile().unwrap().model, "text-embedding-3-large");
        assert!(matches!(
            service
                .generate(GenerateRequest::pinned(
                    vec!["query".into()],
                    InputType::Query,
                    original
                ))
                .await,
            Err(EmbeddingError::SettingsChanged)
        ));
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn rejects_bad_input_and_stale_settings_before_network_call() {
        let service = configured("http://127.0.0.1:1", Provider::Openai);
        assert!(matches!(
            service.generate(request(&[])).await,
            Err(EmbeddingError::Invalid(_))
        ));
        assert!(matches!(
            service.generate(request(&[" "])).await,
            Err(EmbeddingError::Invalid(_))
        ));
        let mut stale = request(&["hello"]);
        stale.expected_settings = Some(ExpectedSettings {
            provider: Provider::Openai,
            model: "text-embedding-3-large".into(),
            dimensions: 2,
        });
        assert!(matches!(
            service.generate(stale).await,
            Err(EmbeddingError::SettingsChanged)
        ));
        for changed in 0..3 {
            let mut profile = service.profile().unwrap();
            match changed {
                0 => profile.provider = Provider::Voyage,
                1 => profile.model = "text-embedding-3-large".into(),
                2 => profile.dimensions += 1,
                _ => unreachable!(),
            }
            assert!(matches!(
                service
                    .generate(GenerateRequest::pinned(
                        vec!["hello".into()],
                        InputType::Query,
                        profile,
                    ))
                    .await,
                Err(EmbeddingError::SettingsChanged)
            ));
        }
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
    }

    #[actix_web::test]
    async fn provider_errors_are_sanitized_and_not_retried() {
        for (status, expected) in [(401, "API key"), (429, "rate limit"), (500, "unavailable")] {
            let (endpoint, mock) = mock_provider(1, move |_, _, _| {
                (status, json!({ "error": "upstream-secret-and-user-text" }))
            });
            let error = configured(&endpoint, Provider::Openai)
                .generate(request(&["hello"]))
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains(expected));
            assert!(!error.to_string().contains("upstream-secret"));
            mock.join().unwrap();
        }
    }

    #[actix_web::test]
    async fn validates_untrusted_provider_shapes_and_body_size() {
        let (endpoint, mock) = mock_provider_encoded(1, true, |_, _, _| {
            (200, json!({"padding":"x".repeat(10000)}))
        });
        assert!(matches!(
            configured(&endpoint, Provider::Openai)
                .generate(request(&["hello"]))
                .await,
            Err(EmbeddingError::InvalidResponse)
        ));
        mock.join().unwrap();
        let responses = [
            json!({ "data": [] }),
            json!({ "data": [{"index": 1, "embedding": [1.0, 2.0]}] }),
            json!({ "data": [{"index": 0, "embedding": [1.0]}] }),
            json!({ "data": [{"index": 0, "embedding": "base64-is-not-float"}] }),
            json!({ "data": [], "padding": "x".repeat(10000) }),
        ];
        for response in responses {
            let (endpoint, mock) = mock_provider(1, move |_, _, _| (200, response.clone()));
            assert!(matches!(
                configured(&endpoint, Provider::Openai)
                    .generate(request(&["hello"]))
                    .await,
                Err(EmbeddingError::InvalidResponse)
            ));
            mock.join().unwrap();
        }
        assert!(validate_embeddings(
            vec![ProviderEmbedding {
                index: 0,
                embedding: vec![f32::INFINITY]
            }],
            1,
            1
        )
        .is_err());
        assert!(validate_embeddings(
            vec![
                ProviderEmbedding {
                    index: 0,
                    embedding: vec![1.0]
                },
                ProviderEmbedding {
                    index: 0,
                    embedding: vec![1.0]
                }
            ],
            2,
            1
        )
        .is_err());
    }

    #[actix_web::test]
    async fn enforces_shared_capacity_and_releases_after_cancellation() {
        let (endpoint, mock) = mock_provider(1, |_, _, _| {
            thread::sleep(Duration::from_millis(100));
            (
                200,
                json!({ "data": [{"index": 0, "embedding": [1.0, 2.0]}] }),
            )
        });
        let service = configured(&endpoint, Provider::Openai);
        let first_service = service.clone();
        let task =
            actix_web::rt::spawn(async move { first_service.generate(request(&["first"])).await });
        while service.inner.in_flight.load(Ordering::Acquire) == 0 {
            actix_web::rt::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(matches!(
            service.generate(request(&["second"])).await,
            Err(EmbeddingError::Busy)
        ));
        task.await.unwrap().unwrap();
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        mock.join().unwrap();

        // Cancelling async I/O releases admission; no blocking provider work remains locally.
        let (endpoint, mock) = mock_provider(1, |_, _, _| {
            thread::sleep(Duration::from_millis(100));
            (
                200,
                json!({ "data": [{"index": 0, "embedding": [1.0, 2.0]}] }),
            )
        });
        let service = configured(&endpoint, Provider::Openai);
        let first_service = service.clone();
        let task =
            actix_web::rt::spawn(async move { first_service.generate(request(&["first"])).await });
        actix_web::rt::time::sleep(Duration::from_millis(30)).await;
        task.abort();
        let _ = task.await;
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn timeout_bounds_the_entire_provider_request() {
        let (endpoint, mock) = mock_provider(1, |_, _, _| {
            thread::sleep(Duration::from_millis(1100));
            (
                200,
                json!({ "data": [{"index": 0, "embedding": [1.0, 2.0]}] }),
            )
        });
        let service = configured(&endpoint, Provider::Openai);
        service
            .update_settings(SettingsUpdate {
                timeout_seconds: Some(1),
                ..SettingsUpdate::default()
            })
            .unwrap();
        assert!(matches!(
            service.generate(request(&["first"])).await,
            Err(EmbeddingError::Timeout)
        ));
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        mock.join().unwrap();
    }

    #[test]
    fn settings_persist_without_keys_and_invalid_updates_are_atomic() {
        let path = std::env::temp_dir().join(format!(
            "vectors-embedding-settings-{}-{}.json",
            std::process::id(),
            CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let service = test_service(Some(path.clone()));
        let settings = service
            .update_settings(SettingsUpdate {
                api_key: Some("synthetic-secret".into()),
                dimensions: Some(512),
                ..SettingsUpdate::default()
            })
            .unwrap();
        let response = serde_json::to_string(&settings).unwrap();
        assert!(!response.contains("synthetic-secret"));
        assert!(!response.contains("api_key"));
        let persisted = fs::read_to_string(&path).unwrap();
        assert!(!persisted.contains("synthetic-secret"));
        assert!(!persisted.contains("api_key"));
        assert_eq!(
            serde_json::from_str::<Config>(&persisted)
                .unwrap()
                .dimensions,
            512
        );
        assert!(service
            .update_settings(SettingsUpdate {
                dimensions: Some(9000),
                api_key: Some("wrong".into()),
                ..SettingsUpdate::default()
            })
            .is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), persisted);
        let settings = service
            .update_settings(SettingsUpdate {
                provider: Some(Provider::Voyage),
                ..SettingsUpdate::default()
            })
            .unwrap();
        assert_eq!(settings.config.model, "voyage-4");
        assert_eq!(settings.config.dimensions, 1024);
        assert!(!settings.configured);
        let settings = service
            .update_settings(SettingsUpdate {
                provider: Some(Provider::Openai),
                clear_api_key: true,
                ..SettingsUpdate::default()
            })
            .unwrap();
        assert!(!settings.configured);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_persistence_does_not_publish_config_or_credentials() {
        let directory = std::env::temp_dir().join(format!(
            "vectors-embedding-failure-{}-{}",
            std::process::id(),
            CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let service = test_service(Some(directory.clone()));
        assert!(matches!(
            service.update_settings(SettingsUpdate {
                api_key: Some("synthetic-secret".into()),
                dimensions: Some(512),
                ..SettingsUpdate::default()
            }),
            Err(EmbeddingError::Persistence)
        ));
        let state = service.settings().unwrap();
        assert_eq!(state.config.dimensions, 1536);
        assert!(!state.configured);
        fs::remove_dir(directory).unwrap();
    }
}
