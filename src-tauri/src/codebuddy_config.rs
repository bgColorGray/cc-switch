//! CodeBuddy Code (CLI) and WorkBuddy (desktop) read custom models from a user-level
//! `models.json` (`~/.codebuddy/` and `~/.workbuddy/`). Both products hot-reload the file.
//!
//! The file is a flat list of models, so one cc-switch provider becomes one entry per model,
//! each tagged with [`MARKER`]. Untagged entries belong to the user or to the product itself
//! (WorkBuddy keeps its local models there) and are never changed.
use crate::app_config::AppType;
use crate::config::{atomic_write_private, get_home_dir};
use crate::error::AppError;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Field that ties a `models.json` entry to the cc-switch provider key that wrote it.
const MARKER: &str = "ccSwitchProvider";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const API_COMPLETIONS: &str = "openai-completions";
const API_RESPONSES: &str = "openai-responses";
/// Native model fields a provider may set directly; they win over the derived ones.
const NATIVE_MODEL_FIELDS: [&str; 9] = [
    "supportsToolCall",
    "supportsImages",
    "supportsReasoning",
    "temperature",
    "reasoning",
    "relatedModels",
    "description",
    "descriptionZh",
    "descriptionEn",
];

// Serializes read-modify-write cycles inside this process; the products only write the file
// when the user edits custom models in their own settings.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Buddy {
    CodeBuddy,
    WorkBuddy,
}

impl Buddy {
    pub(crate) fn from_app(app_type: &AppType) -> Option<Self> {
        match app_type {
            AppType::CodeBuddy => Some(Self::CodeBuddy),
            AppType::WorkBuddy => Some(Self::WorkBuddy),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CodeBuddy => "CodeBuddy",
            Self::WorkBuddy => "WorkBuddy",
        }
    }

    pub(crate) fn data_dir(self) -> PathBuf {
        let (env, dir) = match self {
            Self::CodeBuddy => ("CODEBUDDY_CONFIG_DIR", ".codebuddy"),
            Self::WorkBuddy => ("WORKBUDDY_CONFIG_DIR", ".workbuddy"),
        };
        std::env::var(env)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| get_home_dir().join(dir))
    }

    pub(crate) fn config_path(self) -> PathBuf {
        self.data_dir().join("models.json")
    }
}

fn read_document(buddy: Buddy, path: &Path) -> Result<Map<String, Value>, AppError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(AppError::io(path, error)),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let document: Value = serde_json::from_str(&text).map_err(|error| {
        AppError::Config(format!("Invalid {} models.json: {error}", buddy.label()))
    })?;
    let Value::Object(document) = document else {
        return Err(AppError::Config(format!(
            "{} models.json must be a JSON object",
            buddy.label()
        )));
    };
    if document
        .get("models")
        .is_some_and(|models| !models.is_array())
    {
        return Err(AppError::Config(format!(
            "{} models.json: \"models\" must be an array",
            buddy.label()
        )));
    }
    Ok(document)
}

fn models_of(document: &Map<String, Value>) -> &[Value] {
    document
        .get("models")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn owner_of(entry: &Value) -> Option<&str> {
    entry.get(MARKER).and_then(Value::as_str)
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn api_of(config: &Value) -> &str {
    config
        .get("api")
        .and_then(Value::as_str)
        .unwrap_or(API_COMPLETIONS)
}

/// CodeBuddy wants the full Chat Completions path; Responses takes the base URL.
fn endpoint_url(api: &str, base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if api == API_RESPONSES || base.ends_with(CHAT_COMPLETIONS_PATH) {
        base.to_string()
    } else {
        format!("{base}{CHAT_COMPLETIONS_PATH}")
    }
}

fn base_url_of(entry: &Value) -> String {
    let url = str_at(entry, "/url");
    url.strip_suffix(CHAT_COMPLETIONS_PATH)
        .unwrap_or(url)
        .to_string()
}

/// Rebuild a provider from its tagged entries (used only when the database has no row).
pub(crate) fn get_providers(buddy: Buddy) -> Result<IndexMap<String, Value>, AppError> {
    let document = read_document(buddy, &buddy.config_path())?;
    let mut providers: IndexMap<String, Value> = IndexMap::new();
    for entry in models_of(&document) {
        let (Some(owner), Some(model_id)) =
            (owner_of(entry), entry.get("id").and_then(Value::as_str))
        else {
            continue;
        };
        let provider = providers.entry(owner.to_string()).or_insert_with(|| {
            json!({
                "name": entry.get("vendor").and_then(Value::as_str).unwrap_or(owner),
                "kind": "custom",
                "enabled": true,
                "api": entry.get("api").and_then(Value::as_str).unwrap_or(API_COMPLETIONS),
                "options": {
                    "baseURL": base_url_of(entry),
                    "apiKey": str_at(entry, "/apiKey"),
                },
                "models": {},
            })
        });
        let mut model = json!({
            "name": entry.get("name").and_then(Value::as_str).unwrap_or(model_id),
        });
        for (field, limit) in [("maxInputTokens", "context"), ("maxOutputTokens", "output")] {
            if let Some(value) = entry.get(field).filter(|value| value.is_number()) {
                model["limit"][limit] = value.clone();
            }
        }
        provider["models"][model_id] = model;
    }
    Ok(providers)
}

pub(crate) fn provider_key_exists(buddy: Buddy, id: &str) -> Result<bool, AppError> {
    let document = read_document(buddy, &buddy.config_path())?;
    Ok(models_of(&document)
        .iter()
        .any(|entry| owner_of(entry) == Some(id)))
}

pub(crate) fn validate_provider(buddy: Buddy, id: &str, config: &Value) -> Result<(), AppError> {
    let label = buddy.label();
    if id.is_empty()
        || id
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_'))
    {
        return Err(AppError::InvalidInput(format!(
            "{label} provider key must contain letters, digits, '-' or '_'"
        )));
    }
    if !matches!(api_of(config), API_COMPLETIONS | API_RESPONSES) {
        return Err(AppError::InvalidInput(format!(
            "{label} supports OpenAI Chat Completions or OpenAI Responses only"
        )));
    }
    let base_url = str_at(config, "/options/baseURL");
    if !url::Url::parse(base_url.trim())
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
    {
        return Err(AppError::InvalidInput(format!(
            "Enter a valid {label} endpoint URL"
        )));
    }
    if str_at(config, "/options/apiKey").trim().is_empty() {
        return Err(AppError::InvalidInput("Enter an API key".into()));
    }
    if config
        .pointer("/options/headers")
        .and_then(Value::as_object)
        .is_some_and(|headers| !headers.is_empty())
    {
        return Err(AppError::InvalidInput(format!(
            "{label} models.json cannot send custom request headers"
        )));
    }
    if config
        .get("models")
        .and_then(Value::as_object)
        .is_none_or(|models| models.is_empty() || models.keys().any(|id| id.trim().is_empty()))
    {
        return Err(AppError::InvalidInput(format!(
            "Add at least one {label} model"
        )));
    }
    Ok(())
}

fn native_entry(id: &str, config: &Value, model_id: &str, model: &Value) -> Value {
    let api = api_of(config);
    let mut entry = json!({
        "id": model_id,
        "name": model.get("name").and_then(Value::as_str).filter(|name| !name.trim().is_empty()).unwrap_or(model_id),
        "vendor": config.get("name").and_then(Value::as_str).unwrap_or(id),
        "url": endpoint_url(api, str_at(config, "/options/baseURL")),
        "apiKey": str_at(config, "/options/apiKey").trim(),
        "supportsToolCall": model.get("tool_call").and_then(Value::as_bool).unwrap_or(true),
    });
    if api == API_RESPONSES {
        entry["api"] = json!(API_RESPONSES);
    }
    for (limit, field) in [("context", "maxInputTokens"), ("output", "maxOutputTokens")] {
        if let Some(value) = model
            .pointer(&format!("/limit/{limit}"))
            .filter(|value| value.is_number())
        {
            entry[field] = value.clone();
        }
    }
    if let Some(inputs) = model.pointer("/modalities/input").and_then(Value::as_array) {
        entry["supportsImages"] = json!(inputs.iter().any(|input| input == "image"));
    }
    if let Some(reasoning) = model.get("reasoning").and_then(Value::as_bool) {
        entry["supportsReasoning"] = json!(reasoning);
    }
    for field in NATIVE_MODEL_FIELDS {
        if let Some(value) = model
            .get(field)
            .filter(|value| !value.is_boolean() || field != "reasoning")
        {
            entry[field] = value.clone();
        }
    }
    entry[MARKER] = json!(id);
    entry
}

/// Replace (or with `None` remove) every entry tagged `id`, keeping all other entries.
fn apply(
    buddy: Buddy,
    mut document: Map<String, Value>,
    id: &str,
    config: Option<&Value>,
    create: bool,
) -> Result<Map<String, Value>, AppError> {
    let models = models_of(&document);
    if create && models.iter().any(|entry| owner_of(entry) == Some(id)) {
        return Err(AppError::InvalidInput(format!(
            "{} provider key '{id}' already exists",
            buddy.label()
        )));
    }
    let (owned, mut kept): (Vec<Value>, Vec<Value>) = models
        .iter()
        .cloned()
        .partition(|entry| owner_of(entry) == Some(id));
    let added: Vec<Value> = match config {
        Some(config) => config["models"]
            .as_object()
            .map(|models| {
                models
                    .iter()
                    .map(|(model_id, model)| native_entry(id, config, model_id, model))
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };
    for entry in &added {
        let model_id = str_at(entry, "/id");
        if let Some(clash) = kept.iter().find(|other| str_at(other, "/id") == model_id) {
            let holder = owner_of(clash)
                .map(|owner| format!("provider '{owner}'"))
                .unwrap_or_else(|| "an entry you added yourself".into());
            return Err(AppError::InvalidInput(format!(
                "{} model '{model_id}' is already defined in models.json by {holder}",
                buddy.label()
            )));
        }
    }
    let added_ids: Vec<String> = added
        .iter()
        .map(|entry| str_at(entry, "/id").to_string())
        .collect();
    let removed_ids: Vec<String> = owned
        .iter()
        .map(|entry| str_at(entry, "/id").to_string())
        .filter(|model_id| !added_ids.contains(model_id))
        .collect();
    kept.extend(added);
    document.insert("models".into(), Value::Array(kept));
    // An explicit allow-list hides every model it does not name, so keep ours in it.
    if let Some(Value::Array(available)) = document.get("availableModels") {
        let mut next: Vec<Value> = available
            .iter()
            .filter(|model| {
                !model
                    .as_str()
                    .is_some_and(|model| removed_ids.iter().any(|id| id == model))
            })
            .cloned()
            .collect();
        for model_id in &added_ids {
            if !next
                .iter()
                .any(|model| model.as_str() == Some(model_id.as_str()))
            {
                next.push(json!(model_id));
            }
        }
        document.insert("availableModels".into(), Value::Array(next));
    }
    Ok(document)
}

fn write_document(path: &Path, document: &Map<String, Value>) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    }
    let mut text = serde_json::to_string_pretty(document)
        .map_err(|error| AppError::Config(error.to_string()))?;
    text.push('\n');
    atomic_write_private(path, text.as_bytes())
}

/// Write the provider's models (or remove them with `None`), then run `commit`; the file is
/// restored when `commit` fails.
pub(crate) fn save_and_commit<T>(
    buddy: Buddy,
    id: &str,
    config: Option<Value>,
    create: bool,
    commit: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    save_path_and_commit(buddy, &buddy.config_path(), id, config, create, commit)
}

fn save_path_and_commit<T>(
    buddy: Buddy,
    path: &Path,
    id: &str,
    config: Option<Value>,
    create: bool,
    commit: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    if let Some(config) = &config {
        validate_provider(buddy, id, config)?;
    }
    if config.is_none() && !path.exists() {
        return commit();
    }
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let document = read_document(buddy, path)?;
    let next = apply(buddy, document, id, config.as_ref(), create)?;
    crate::mcode_config::write_and_commit(path, || write_document(path, &next), commit)
}

pub(crate) fn set_provider(buddy: Buddy, id: &str, config: Value) -> Result<(), AppError> {
    save_and_commit(buddy, id, Some(config), false, || Ok(()))
}

pub(crate) fn remove_provider(buddy: Buddy, id: &str) -> Result<(), AppError> {
    save_and_commit(buddy, id, None, false, || Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Value {
        json!({
            "name": "My Gateway",
            "kind": "custom",
            "enabled": true,
            "api": "openai-completions",
            "options": { "baseURL": "http://gateway.local:8788/v1/", "apiKey": "sk-test" },
            "models": {
                "glm-5.3": { "name": "GLM 5.3", "limit": { "context": 200000, "output": 32000 } },
                "deepseek-v4.1-flash": { "name": "", "modalities": { "input": ["text", "image"] } }
            }
        })
    }

    fn document(text: &str) -> Map<String, Value> {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn writes_one_tagged_entry_per_model_with_full_chat_url() {
        let next = apply(Buddy::CodeBuddy, Map::new(), "gw", Some(&provider()), true).unwrap();
        let models = next["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "glm-5.3");
        assert_eq!(models[0]["name"], "GLM 5.3");
        assert_eq!(models[0]["vendor"], "My Gateway");
        assert_eq!(
            models[0]["url"],
            "http://gateway.local:8788/v1/chat/completions"
        );
        assert_eq!(models[0]["apiKey"], "sk-test");
        assert_eq!(models[0]["maxInputTokens"], 200000);
        assert_eq!(models[0]["maxOutputTokens"], 32000);
        assert_eq!(models[0]["supportsToolCall"], true);
        assert_eq!(models[0][MARKER], "gw");
        assert!(models[0].get("api").is_none());
        assert_eq!(models[1]["name"], "deepseek-v4.1-flash");
        assert_eq!(models[1]["supportsImages"], true);
        assert!(next.get("availableModels").is_none());
    }

    #[test]
    fn responses_api_keeps_the_base_url() {
        let mut config = provider();
        config["api"] = json!("openai-responses");
        let next = apply(Buddy::WorkBuddy, Map::new(), "gw", Some(&config), true).unwrap();
        assert_eq!(next["models"][0]["api"], "openai-responses");
        assert_eq!(next["models"][0]["url"], "http://gateway.local:8788/v1");
    }

    #[test]
    fn keeps_untagged_entries_and_other_fields() {
        let existing = document(
            r#"{"models":[{"id":"local-qwen","local":true}],"availableModels":["local-qwen"],"other":1}"#,
        );
        let next = apply(Buddy::WorkBuddy, existing, "gw", Some(&provider()), true).unwrap();
        let ids: Vec<&str> = next["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["local-qwen", "glm-5.3", "deepseek-v4.1-flash"]);
        assert_eq!(
            next["availableModels"],
            json!(["local-qwen", "glm-5.3", "deepseek-v4.1-flash"])
        );
        assert_eq!(next["other"], 1);
    }

    #[test]
    fn update_replaces_and_remove_drops_only_own_entries() {
        let existing = document(r#"{"models":[{"id":"mine"}],"availableModels":["mine"]}"#);
        let first = apply(Buddy::CodeBuddy, existing, "gw", Some(&provider()), true).unwrap();
        let mut config = provider();
        config["models"] = json!({ "glm-5.3": { "name": "GLM" } });
        let second = apply(Buddy::CodeBuddy, first, "gw", Some(&config), false).unwrap();
        assert_eq!(second["models"].as_array().unwrap().len(), 2);
        assert_eq!(second["availableModels"], json!(["mine", "glm-5.3"]));
        let removed = apply(Buddy::CodeBuddy, second, "gw", None, false).unwrap();
        assert_eq!(removed["models"], json!([{ "id": "mine" }]));
        assert_eq!(removed["availableModels"], json!(["mine"]));
    }

    #[test]
    fn refuses_model_ids_owned_by_someone_else_and_duplicate_keys() {
        let existing = document(r#"{"models":[{"id":"glm-5.3"}]}"#);
        let error = apply(Buddy::CodeBuddy, existing, "gw", Some(&provider()), true).unwrap_err();
        assert!(error.to_string().contains("added yourself"), "{error}");
        let mine = apply(Buddy::CodeBuddy, Map::new(), "gw", Some(&provider()), true).unwrap();
        assert!(apply(
            Buddy::CodeBuddy,
            mine.clone(),
            "gw",
            Some(&provider()),
            true
        )
        .is_err());
        let error = apply(Buddy::CodeBuddy, mine, "other", Some(&provider()), true).unwrap_err();
        assert!(error.to_string().contains("provider 'gw'"), "{error}");
    }

    #[test]
    fn validation_rejects_unsupported_shapes() {
        let ok = provider();
        assert!(validate_provider(Buddy::CodeBuddy, "gw", &ok).is_ok());
        let broken: [fn(&mut Value); 5] = [
            |config| config["api"] = json!("anthropic-messages"),
            |config| config["options"]["baseURL"] = json!("not a url"),
            |config| config["options"]["apiKey"] = json!(" "),
            |config| config["options"]["headers"] = json!({ "X-A": "1" }),
            |config| config["models"] = json!({}),
        ];
        for (index, breaks) in broken.iter().enumerate() {
            let mut config = ok.clone();
            breaks(&mut config);
            assert!(
                validate_provider(Buddy::CodeBuddy, "gw", &config).is_err(),
                "case {index}"
            );
        }
        assert!(validate_provider(Buddy::CodeBuddy, "bad key", &ok).is_err());
    }

    #[test]
    fn round_trips_through_the_file_and_rolls_back_on_failed_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        fs::write(&path, r#"{"models":[{"id":"mine"}]}"#).unwrap();
        save_path_and_commit(
            Buddy::CodeBuddy,
            &path,
            "gw",
            Some(provider()),
            true,
            || Ok(()),
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"ccSwitchProvider\": \"gw\""));
        let failed: Result<(), AppError> =
            save_path_and_commit(Buddy::CodeBuddy, &path, "gw", None, false, || {
                Err(AppError::Message("db down".into()))
            });
        assert!(failed.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let document = read_document(Buddy::CodeBuddy, &path).unwrap();
        assert_eq!(models_of(&document).len(), 3);
    }

    #[test]
    fn rejects_malformed_files_instead_of_overwriting_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        fs::write(&path, "{ not json").unwrap();
        assert!(save_path_and_commit(
            Buddy::CodeBuddy,
            &path,
            "gw",
            Some(provider()),
            true,
            || Ok(())
        )
        .is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
