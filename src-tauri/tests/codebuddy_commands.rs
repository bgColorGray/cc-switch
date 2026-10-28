use cc_switch_lib::{AppType, Provider, ProviderService};
use serde_json::{json, Value};
use std::{ffi::OsString, fs, path::PathBuf};

#[path = "support.rs"]
mod support;
use support::{create_test_state, reset_test_fs, test_mutex};

/// Points one product's config directory at a temp dir for the test's lifetime.
struct BuddyHome {
    directory: tempfile::TempDir,
    variable: &'static str,
    previous: Option<OsString>,
}
impl BuddyHome {
    fn new(variable: &'static str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let previous = std::env::var_os(variable);
        std::env::set_var(variable, directory.path());
        Self {
            directory,
            variable,
            previous,
        }
    }
    fn models_json(&self) -> PathBuf {
        self.directory.path().join("models.json")
    }
    fn read(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.models_json()).unwrap()).unwrap()
    }
}
impl Drop for BuddyHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.variable, value),
            None => std::env::remove_var(self.variable),
        }
    }
}

fn provider(id: &str, model: &str) -> Provider {
    Provider::with_id(
        id.into(),
        "Gateway".into(),
        json!({
            "name": "Gateway",
            "kind": "custom",
            "enabled": true,
            "api": "openai-completions",
            "options": {"baseURL": "http://gateway.example:8788/v1", "apiKey": "test-value"},
            "models": {model: {"name": model, "limit": {"context": 128000}}}
        }),
        None,
    )
}

fn ids(document: &Value) -> Vec<String> {
    document["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect()
}

fn managed(state: &cc_switch_lib::AppState, app: AppType, id: &str) -> Option<bool> {
    ProviderService::list(state, app).unwrap()[id]
        .meta
        .as_ref()
        .and_then(|meta| meta.live_config_managed)
}

#[test]
fn codebuddy_provider_lifecycle_keeps_the_users_own_models() {
    let _guard = test_mutex().lock().unwrap();
    reset_test_fs();
    let home = BuddyHome::new("CODEBUDDY_CONFIG_DIR");
    let state = create_test_state().unwrap();
    fs::write(
        home.models_json(),
        r#"{"models":[{"id":"mine","url":"http://localhost:11434/v1/chat/completions"}],"availableModels":["mine"]}"#,
    )
    .unwrap();

    ProviderService::add(&state, AppType::CodeBuddy, provider("gw", "glm-5.3"), false).unwrap();
    assert_eq!(ids(&home.read()), ["mine"]);
    assert_eq!(managed(&state, AppType::CodeBuddy, "gw"), Some(false));

    ProviderService::switch(&state, AppType::CodeBuddy, "gw").unwrap();
    let document = home.read();
    assert_eq!(ids(&document), ["mine", "glm-5.3"]);
    assert_eq!(document["availableModels"], json!(["mine", "glm-5.3"]));
    assert_eq!(
        document["models"][1]["url"],
        "http://gateway.example:8788/v1/chat/completions"
    );
    assert_eq!(document["models"][1]["maxInputTokens"], 128000);
    assert_eq!(managed(&state, AppType::CodeBuddy, "gw"), Some(true));

    let mut edited = ProviderService::list(&state, AppType::CodeBuddy).unwrap()["gw"].clone();
    edited.settings_config["models"] = json!({"deepseek-v4.1-flash": {"name": "Flash"}});
    ProviderService::update(&state, AppType::CodeBuddy, None, edited).unwrap();
    assert_eq!(ids(&home.read()), ["mine", "deepseek-v4.1-flash"]);

    assert!(
        ProviderService::add(&state, AppType::CodeBuddy, provider("gw", "x"), true).is_err(),
        "duplicate provider key"
    );
    assert!(
        ProviderService::add(&state, AppType::CodeBuddy, provider("other", "mine"), true).is_err(),
        "model id owned by the user"
    );

    ProviderService::remove_from_live_config(&state, AppType::CodeBuddy, "gw").unwrap();
    assert_eq!(ids(&home.read()), ["mine"]);
    assert_eq!(home.read()["availableModels"], json!(["mine"]));
    assert_eq!(managed(&state, AppType::CodeBuddy, "gw"), Some(false));

    ProviderService::switch(&state, AppType::CodeBuddy, "gw").unwrap();
    ProviderService::delete(&state, AppType::CodeBuddy, "gw").unwrap();
    assert_eq!(ids(&home.read()), ["mine"]);
    assert!(!ProviderService::list(&state, AppType::CodeBuddy)
        .unwrap()
        .contains_key("gw"));
}

#[test]
fn workbuddy_writes_its_own_file_and_recovers_tagged_entries() {
    let _guard = test_mutex().lock().unwrap();
    reset_test_fs();
    let codebuddy = BuddyHome::new("CODEBUDDY_CONFIG_DIR");
    let workbuddy = BuddyHome::new("WORKBUDDY_CONFIG_DIR");
    let state = create_test_state().unwrap();

    ProviderService::add(&state, AppType::WorkBuddy, provider("gw", "glm-5.3"), true).unwrap();
    assert_eq!(ids(&workbuddy.read()), ["glm-5.3"]);
    assert!(!codebuddy.models_json().exists());
    assert!(workbuddy.read().get("availableModels").is_none());

    // An entry tagged by another install shows up as an added provider.
    let mut document = workbuddy.read();
    document["models"].as_array_mut().unwrap().push(
        json!({"id": "kimi-k3", "name": "Kimi", "vendor": "Restored",
                     "url": "https://restored.example/v1/chat/completions",
                     "apiKey": "test-value", "ccSwitchProvider": "restored"}),
    );
    fs::write(workbuddy.models_json(), document.to_string()).unwrap();
    let providers = ProviderService::list(&state, AppType::WorkBuddy).unwrap();
    let restored = &providers["restored"];
    assert_eq!(restored.name, "Restored");
    assert_eq!(
        restored.settings_config["options"]["baseURL"],
        "https://restored.example/v1"
    );
    assert_eq!(managed(&state, AppType::WorkBuddy, "restored"), Some(true));
}
