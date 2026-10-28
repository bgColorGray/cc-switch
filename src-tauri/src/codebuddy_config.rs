//! CodeBuddy Code (CLI) and WorkBuddy (desktop) read custom models from a user-level
//! `models.json` (`~/.codebuddy/` and `~/.workbuddy/`). Both products hot-reload the file.
//! One cc-switch app manages both: every write goes to the two files together.
//!
//! The file is a flat list of models, so one cc-switch provider becomes one entry per model,
//! each tagged with [`MARKER`]. Untagged entries belong to the user or to the product itself
//! (WorkBuddy keeps its local models there) and are never changed.
//!
//! While any provider is added, `availableModels` lists only the custom models, so the
//! products' built-in (official) models leave the model picker and cannot be picked by
//! mistake. Both products expose custom models as `custom-local:<id>` (keeping `<id>` as an
//! alias, so `--model <id>` still resolves to them once the built-in one is hidden). The list
//! the user had before is kept in a sidecar file and restored when the last provider leaves.
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
/// Prefix both products give custom model ids in their model lists.
const CUSTOM_LOCAL_PREFIX: &str = "custom-local:";
/// Sidecar next to `models.json` holding the user's own `availableModels` (or `null`).
const SAVED_ALLOW_LIST_FILE: &str = ".cc-switch-available-models.json";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const API_COMPLETIONS: &str = "openai-completions";
const API_RESPONSES: &str = "openai-responses";
const LABEL: &str = "CodeBuddy / WorkBuddy";
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
enum Buddy {
    CodeBuddy,
    WorkBuddy,
}

impl Buddy {
    const ALL: [Buddy; 2] = [Buddy::CodeBuddy, Buddy::WorkBuddy];

    fn data_dir(self) -> PathBuf {
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
}

/// CodeBuddy's directory; the app's "open config folder" and skills path point here.
pub(crate) fn data_dir() -> PathBuf {
    Buddy::CodeBuddy.data_dir()
}

pub(crate) fn config_path() -> PathBuf {
    data_dir().join("models.json")
}

fn model_dirs() -> Vec<PathBuf> {
    Buddy::ALL.iter().map(|buddy| buddy.data_dir()).collect()
}

fn read_document(path: &Path) -> Result<Map<String, Value>, AppError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(AppError::io(path, error)),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let document: Value = serde_json::from_str(&text)
        .map_err(|error| AppError::Config(format!("Invalid {}: {error}", path.display())))?;
    let Value::Object(document) = document else {
        return Err(AppError::Config(format!(
            "{} must be a JSON object",
            path.display()
        )));
    };
    if document
        .get("models")
        .is_some_and(|models| !models.is_array())
    {
        return Err(AppError::Config(format!(
            "{}: \"models\" must be an array",
            path.display()
        )));
    }
    Ok(document)
}

/// The user's saved allow-list: `None` when no sidecar exists, `Some(Value::Null)` when the
/// user had none.
fn read_saved_allow_list(path: &Path) -> Result<Option<Value>, AppError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let saved: Value = serde_json::from_str(&text).map_err(|error| {
                AppError::Config(format!("Invalid {}: {error}", path.display()))
            })?;
            Ok(Some(
                saved.get("availableModels").cloned().unwrap_or(Value::Null),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::io(path, error)),
    }
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

fn collect_providers(
    document: &Map<String, Value>,
    providers: &mut IndexMap<String, Value>,
    seen: &mut Vec<String>,
) {
    for entry in models_of(document) {
        let (Some(owner), Some(model_id)) =
            (owner_of(entry), entry.get("id").and_then(Value::as_str))
        else {
            continue;
        };
        // A provider found in an earlier file wins; its models are not merged twice.
        if seen.iter().any(|id| id == owner) {
            continue;
        }
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
    seen.extend(providers.keys().cloned());
}

/// Rebuild providers from their tagged entries in either file (used only when the database
/// has no row).
pub(crate) fn get_providers() -> Result<IndexMap<String, Value>, AppError> {
    let mut providers = IndexMap::new();
    let mut seen = Vec::new();
    for dir in model_dirs() {
        let document = read_document(&dir.join("models.json"))?;
        collect_providers(&document, &mut providers, &mut seen);
    }
    Ok(providers)
}

pub(crate) fn provider_key_exists(id: &str) -> Result<bool, AppError> {
    for dir in model_dirs() {
        let document = read_document(&dir.join("models.json"))?;
        if models_of(&document)
            .iter()
            .any(|entry| owner_of(entry) == Some(id))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn validate_provider(id: &str, config: &Value) -> Result<(), AppError> {
    if id.is_empty()
        || id
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_'))
    {
        return Err(AppError::InvalidInput(format!(
            "{LABEL} provider key must contain letters, digits, '-' or '_'"
        )));
    }
    if !matches!(api_of(config), API_COMPLETIONS | API_RESPONSES) {
        return Err(AppError::InvalidInput(format!(
            "{LABEL} supports OpenAI Chat Completions or OpenAI Responses only"
        )));
    }
    let base_url = str_at(config, "/options/baseURL");
    if !url::Url::parse(base_url.trim())
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
    {
        return Err(AppError::InvalidInput(format!(
            "Enter a valid {LABEL} endpoint URL"
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
            "{LABEL} models.json cannot send custom request headers"
        )));
    }
    if config
        .get("models")
        .and_then(Value::as_object)
        .is_none_or(|models| models.is_empty() || models.keys().any(|id| id.trim().is_empty()))
    {
        return Err(AppError::InvalidInput(format!(
            "Add at least one {LABEL} model"
        )));
    }
    Ok(())
}

fn native_entry(id: &str, config: &Value, model_id: &str, model: &Value) -> Value {
    let api = api_of(config);
    let name = model
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(model_id);
    let mut entry = json!({
        "id": model_id,
        "name": name,
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
        // OpenCode-style `reasoning: bool` was mapped above; CodeBuddy's own is an object.
        if let Some(value) = model
            .get(field)
            .filter(|value| field != "reasoning" || !value.is_boolean())
        {
            entry[field] = value.clone();
        }
    }
    entry[MARKER] = json!(id);
    entry
}

/// What to do with the sidecar holding the user's own allow-list.
#[derive(Debug, PartialEq)]
enum SavedAllowList {
    Keep,
    Save(Value),
    Remove,
}

/// Replace (or with `None` remove) every entry tagged `id`, keeping all other entries, then
/// recompute `availableModels`. `saved` is the sidecar's content (see [`read_saved_allow_list`]).
fn apply(
    file: &Path,
    mut document: Map<String, Value>,
    id: &str,
    config: Option<&Value>,
    create: bool,
    saved: Option<&Value>,
) -> Result<(Map<String, Value>, SavedAllowList), AppError> {
    let models = models_of(&document);
    if create && models.iter().any(|entry| owner_of(entry) == Some(id)) {
        return Err(AppError::InvalidInput(format!(
            "{LABEL} provider key '{id}' already exists"
        )));
    }
    let mut kept: Vec<Value> = models
        .iter()
        .filter(|entry| owner_of(entry) != Some(id))
        .cloned()
        .collect();
    let added: Vec<Value> = config
        .and_then(|config| {
            config["models"].as_object().map(|models| {
                models
                    .iter()
                    .map(|(model_id, model)| native_entry(id, config, model_id, model))
                    .collect()
            })
        })
        .unwrap_or_default();
    for entry in &added {
        let model_id = str_at(entry, "/id");
        if let Some(clash) = kept.iter().find(|other| str_at(other, "/id") == model_id) {
            let holder = owner_of(clash)
                .map(|owner| format!("provider '{owner}'"))
                .unwrap_or_else(|| "an entry you added yourself".into());
            return Err(AppError::InvalidInput(format!(
                "Model '{model_id}' is already defined in {} by {holder}",
                file.display()
            )));
        }
    }
    kept.extend(added);
    let has_ours = kept.iter().any(|entry| owner_of(entry).is_some());
    let allow_list: Vec<Value> = kept
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .map(|model_id| json!(format!("{CUSTOM_LOCAL_PREFIX}{model_id}")))
        .collect();
    document.insert("models".into(), Value::Array(kept));
    let sidecar = if has_ours {
        let action = match saved {
            Some(_) => SavedAllowList::Keep,
            None => SavedAllowList::Save(
                document
                    .get("availableModels")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
        };
        document.insert("availableModels".into(), Value::Array(allow_list));
        action
    } else {
        match saved {
            Some(Value::Null) => {
                document.remove("availableModels");
                SavedAllowList::Remove
            }
            Some(previous) => {
                document.insert("availableModels".into(), previous.clone());
                SavedAllowList::Remove
            }
            None => SavedAllowList::Keep,
        }
    };
    Ok((document, sidecar))
}

fn write_json(path: &Path, value: &Value) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    }
    let mut text =
        serde_json::to_string_pretty(value).map_err(|error| AppError::Config(error.to_string()))?;
    text.push('\n');
    atomic_write_private(path, text.as_bytes())
}

/// One file's pending write.
struct Plan {
    path: PathBuf,
    sidecar_path: PathBuf,
    document: Map<String, Value>,
    sidecar: SavedAllowList,
}

impl Plan {
    fn write(&self) -> Result<(), AppError> {
        // The sidecar goes first: once models.json hides the built-in models, the way back
        // must already be on disk.
        match &self.sidecar {
            SavedAllowList::Keep => {}
            SavedAllowList::Save(previous) => {
                write_json(&self.sidecar_path, &json!({ "availableModels": previous }))?
            }
            SavedAllowList::Remove => {}
        }
        write_json(&self.path, &Value::Object(self.document.clone()))
    }

    /// Runs only after the commit succeeded, so a rolled-back write still finds the sidecar.
    fn finish(&self) -> Result<(), AppError> {
        if self.sidecar != SavedAllowList::Remove {
            return Ok(());
        }
        match fs::remove_file(&self.sidecar_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::io(&self.sidecar_path, error)),
        }
    }
}

type Commit<'a, T> = Box<dyn FnOnce() -> Result<T, AppError> + 'a>;

/// Write each plan, then commit; a failure restores every `models.json` written so far.
fn write_plans_and_commit<'a, T>(plans: &'a [Plan], commit: Commit<'a, T>) -> Result<T, AppError> {
    match plans.split_first() {
        None => commit(),
        Some((plan, rest)) => crate::mcode_config::write_and_commit(
            &plan.path,
            || plan.write(),
            || write_plans_and_commit(rest, commit),
        ),
    }
}

/// Write the provider's models to both products (or remove them with `None`), then run
/// `commit`; the files are restored when anything fails.
pub(crate) fn save_and_commit<T>(
    id: &str,
    config: Option<Value>,
    create: bool,
    commit: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    save_dirs_and_commit(&model_dirs(), id, config, create, commit)
}

fn save_dirs_and_commit<T>(
    dirs: &[PathBuf],
    id: &str,
    config: Option<Value>,
    create: bool,
    commit: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    if let Some(config) = &config {
        validate_provider(id, config)?;
    }
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut plans = Vec::new();
    for dir in dirs {
        let path = dir.join("models.json");
        if config.is_none() && !path.exists() {
            continue;
        }
        let sidecar_path = dir.join(SAVED_ALLOW_LIST_FILE);
        let saved = read_saved_allow_list(&sidecar_path)?;
        let (document, sidecar) = apply(
            &path,
            read_document(&path)?,
            id,
            config.as_ref(),
            create,
            saved.as_ref(),
        )?;
        plans.push(Plan {
            path,
            sidecar_path,
            document,
            sidecar,
        });
    }
    let result = write_plans_and_commit(&plans, Box::new(commit))?;
    for plan in &plans {
        if let Err(error) = plan.finish() {
            log::warn!("Could not remove {}: {error}", plan.sidecar_path.display());
        }
    }
    Ok(result)
}

pub(crate) fn set_provider(id: &str, config: Value) -> Result<(), AppError> {
    save_and_commit(id, Some(config), false, || Ok(()))
}

pub(crate) fn remove_provider(id: &str) -> Result<(), AppError> {
    save_and_commit(id, None, false, || Ok(()))
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

    fn run(
        document: Map<String, Value>,
        id: &str,
        config: Option<&Value>,
        saved: Option<&Value>,
    ) -> (Map<String, Value>, SavedAllowList) {
        apply(Path::new("models.json"), document, id, config, false, saved).unwrap()
    }

    #[test]
    fn writes_one_tagged_entry_per_model_with_full_chat_url() {
        let (next, _) = run(Map::new(), "gw", Some(&provider()), None);
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
    }

    #[test]
    fn responses_api_keeps_the_base_url() {
        let mut config = provider();
        config["api"] = json!("openai-responses");
        let (next, _) = run(Map::new(), "gw", Some(&config), None);
        assert_eq!(next["models"][0]["api"], "openai-responses");
        assert_eq!(next["models"][0]["url"], "http://gateway.local:8788/v1");
    }

    #[test]
    fn hides_built_in_models_and_restores_the_users_allow_list() {
        let existing = document(
            r#"{"models":[{"id":"local-qwen","local":true}],"availableModels":["hy3"],"other":1}"#,
        );
        let (added, sidecar) = run(existing, "gw", Some(&provider()), None);
        assert_eq!(
            added["availableModels"],
            json!([
                "custom-local:local-qwen",
                "custom-local:glm-5.3",
                "custom-local:deepseek-v4.1-flash"
            ])
        );
        assert_eq!(sidecar, SavedAllowList::Save(json!(["hy3"])));
        assert_eq!(added["other"], 1);

        // A second save keeps the sidecar that already holds the user's list.
        let saved = json!(["hy3"]);
        let (again, sidecar) = run(added, "gw", Some(&provider()), Some(&saved));
        assert_eq!(sidecar, SavedAllowList::Keep);

        let (removed, sidecar) = run(again, "gw", None, Some(&saved));
        assert_eq!(removed["availableModels"], json!(["hy3"]));
        assert_eq!(
            removed["models"],
            json!([{ "id": "local-qwen", "local": true }])
        );
        assert_eq!(sidecar, SavedAllowList::Remove);
    }

    #[test]
    fn removing_the_last_provider_drops_an_allow_list_the_user_never_had() {
        let (added, sidecar) = run(Map::new(), "gw", Some(&provider()), None);
        assert_eq!(sidecar, SavedAllowList::Save(Value::Null));
        let (removed, sidecar) = run(added, "gw", None, Some(&Value::Null));
        assert!(removed.get("availableModels").is_none());
        assert_eq!(sidecar, SavedAllowList::Remove);
    }

    #[test]
    fn another_provider_keeps_the_built_in_models_hidden() {
        let mut other = provider();
        other["models"] = json!({ "kimi-k3": { "name": "Kimi" } });
        let (one, _) = run(Map::new(), "gw", Some(&provider()), None);
        let (two, _) = run(one, "other", Some(&other), Some(&Value::Null));
        let (left, sidecar) = run(two, "gw", None, Some(&Value::Null));
        assert_eq!(left["availableModels"], json!(["custom-local:kimi-k3"]));
        assert_eq!(sidecar, SavedAllowList::Keep);
    }

    #[test]
    fn refuses_model_ids_owned_by_someone_else_and_duplicate_keys() {
        let path = Path::new("models.json");
        let existing = document(r#"{"models":[{"id":"glm-5.3"}]}"#);
        let error = apply(path, existing, "gw", Some(&provider()), true, None).unwrap_err();
        assert!(error.to_string().contains("added yourself"), "{error}");
        let (mine, _) = run(Map::new(), "gw", Some(&provider()), None);
        assert!(apply(path, mine.clone(), "gw", Some(&provider()), true, None).is_err());
        let error = apply(path, mine, "other", Some(&provider()), true, None).unwrap_err();
        assert!(error.to_string().contains("provider 'gw'"), "{error}");
    }

    #[test]
    fn validation_rejects_unsupported_shapes() {
        let ok = provider();
        assert!(validate_provider("gw", &ok).is_ok());
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
            assert!(validate_provider("gw", &config).is_err(), "case {index}");
        }
        assert!(validate_provider("bad key", &ok).is_err());
    }

    #[test]
    fn writes_both_files_and_rolls_both_back_on_failed_commit() {
        let root = tempfile::tempdir().unwrap();
        let dirs = [root.path().join("code"), root.path().join("work")];
        fs::create_dir_all(&dirs[0]).unwrap();
        fs::write(dirs[0].join("models.json"), r#"{"models":[{"id":"mine"}]}"#).unwrap();
        save_dirs_and_commit(&dirs, "gw", Some(provider()), true, || Ok(())).unwrap();
        let code = fs::read_to_string(dirs[0].join("models.json")).unwrap();
        let work = fs::read_to_string(dirs[1].join("models.json")).unwrap();
        assert!(code.contains("\"ccSwitchProvider\": \"gw\""));
        assert!(work.contains("\"ccSwitchProvider\": \"gw\""));
        assert!(dirs[0].join(SAVED_ALLOW_LIST_FILE).exists());

        let failed: Result<(), AppError> = save_dirs_and_commit(&dirs, "gw", None, false, || {
            Err(AppError::Message("db down".into()))
        });
        assert!(failed.is_err());
        assert_eq!(
            fs::read_to_string(dirs[0].join("models.json")).unwrap(),
            code
        );
        assert_eq!(
            fs::read_to_string(dirs[1].join("models.json")).unwrap(),
            work
        );
        assert!(
            dirs[0].join(SAVED_ALLOW_LIST_FILE).exists(),
            "rollback keeps the sidecar"
        );

        save_dirs_and_commit(&dirs, "gw", None, false, || Ok(())).unwrap();
        let restored = read_document(&dirs[0].join("models.json")).unwrap();
        assert_eq!(restored["models"], json!([{ "id": "mine" }]));
        assert!(restored.get("availableModels").is_none());
        assert!(!dirs[0].join(SAVED_ALLOW_LIST_FILE).exists());
        assert!(!dirs[1].join(SAVED_ALLOW_LIST_FILE).exists());
    }

    #[test]
    fn rejects_malformed_files_instead_of_overwriting_them() {
        let root = tempfile::tempdir().unwrap();
        let dirs = [root.path().join("code"), root.path().join("work")];
        fs::create_dir_all(&dirs[1]).unwrap();
        fs::write(dirs[1].join("models.json"), "{ not json").unwrap();
        assert!(save_dirs_and_commit(&dirs, "gw", Some(provider()), true, || Ok(())).is_err());
        assert!(!dirs[0].join("models.json").exists());
        assert_eq!(
            fs::read_to_string(dirs[1].join("models.json")).unwrap(),
            "{ not json"
        );
    }
}
