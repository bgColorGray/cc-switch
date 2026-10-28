use cc_switch_lib::{AppType, Provider, ProviderService};
use serde_json::{json, Value};
use std::{ffi::OsString, fs, path::PathBuf};

const OFFICIAL: &str = "codebuddy-official";

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

fn managed(state: &cc_switch_lib::AppState, id: &str) -> Option<bool> {
    ProviderService::list(state, AppType::CodeBuddy).unwrap()[id]
        .meta
        .as_ref()
        .and_then(|meta| meta.live_config_managed)
}

fn current(state: &cc_switch_lib::AppState) -> String {
    ProviderService::current(state, AppType::CodeBuddy).unwrap()
}

#[test]
fn one_provider_at_a_time_drives_both_products_and_official_restores_them() {
    let _guard = test_mutex().lock().unwrap();
    reset_test_fs();
    let code = BuddyHome::new("CODEBUDDY_CONFIG_DIR");
    let work = BuddyHome::new("WORKBUDDY_CONFIG_DIR");
    let state = create_test_state().unwrap();
    fs::write(
        code.models_json(),
        r#"{"models":[{"id":"mine","url":"http://localhost:11434/v1/chat/completions"}],"availableModels":["mine","hy3"]}"#,
    )
    .unwrap();

    let official = ProviderService::list(&state, AppType::CodeBuddy).unwrap()[OFFICIAL].clone();
    assert_eq!(official.category.as_deref(), Some("official"));
    assert_eq!(current(&state), OFFICIAL);

    // Adding never switches, even when asked to write the live files.
    ProviderService::add(&state, AppType::CodeBuddy, provider("gw", "glm-5.3"), true).unwrap();
    assert_eq!(ids(&code.read()), ["mine"]);
    assert!(!work.models_json().exists());
    assert_eq!(managed(&state, "gw"), Some(false));
    assert_eq!(current(&state), OFFICIAL);

    ProviderService::switch(&state, AppType::CodeBuddy, "gw").unwrap();
    let document = code.read();
    assert_eq!(ids(&document), ["mine", "glm-5.3"]);
    assert_eq!(
        document["availableModels"],
        json!(["custom-local:mine", "custom-local:glm-5.3"])
    );
    assert_eq!(
        document["models"][1]["url"],
        "http://gateway.example:8788/v1/chat/completions"
    );
    let workbuddy = work.read();
    assert_eq!(ids(&workbuddy), ["glm-5.3"]);
    assert_eq!(
        workbuddy["availableModels"],
        json!(["custom-local:glm-5.3"])
    );
    assert_eq!(current(&state), "gw");
    assert_eq!(managed(&state, "gw"), Some(true));
    assert_eq!(managed(&state, OFFICIAL), Some(false));

    ProviderService::add(
        &state,
        AppType::CodeBuddy,
        provider("other", "kimi-k3"),
        false,
    )
    .unwrap();
    ProviderService::switch(&state, AppType::CodeBuddy, "other").unwrap();
    assert_eq!(ids(&code.read()), ["mine", "kimi-k3"]);
    assert_eq!(ids(&work.read()), ["kimi-k3"]);
    assert_eq!(current(&state), "other");
    assert_eq!(managed(&state, "gw"), Some(false));

    let mut edited = ProviderService::list(&state, AppType::CodeBuddy).unwrap()["other"].clone();
    edited.settings_config["models"] = json!({"deepseek-v4.1-flash": {"name": "Flash"}});
    ProviderService::update(&state, AppType::CodeBuddy, None, edited).unwrap();
    assert_eq!(ids(&code.read()), ["mine", "deepseek-v4.1-flash"]);
    assert_eq!(ids(&work.read()), ["deepseek-v4.1-flash"]);

    assert!(
        ProviderService::add(&state, AppType::CodeBuddy, provider("gw", "x"), false).is_err(),
        "duplicate provider key"
    );
    assert!(ProviderService::delete(&state, AppType::CodeBuddy, OFFICIAL).is_err());
    assert!(ProviderService::update(&state, AppType::CodeBuddy, None, official).is_err());

    ProviderService::switch(&state, AppType::CodeBuddy, OFFICIAL).unwrap();
    let restored = code.read();
    assert_eq!(ids(&restored), ["mine"]);
    assert_eq!(restored["availableModels"], json!(["mine", "hy3"]));
    let workbuddy = work.read();
    assert_eq!(ids(&workbuddy), Vec::<String>::new());
    assert!(workbuddy.get("availableModels").is_none());
    assert_eq!(current(&state), OFFICIAL);
    assert_eq!(managed(&state, OFFICIAL), Some(true));
    assert_eq!(managed(&state, "other"), Some(false));

    ProviderService::delete(&state, AppType::CodeBuddy, "gw").unwrap();
    assert!(!ProviderService::list(&state, AppType::CodeBuddy)
        .unwrap()
        .contains_key("gw"));
}

#[test]
fn tagged_entries_in_either_file_are_recovered() {
    let _guard = test_mutex().lock().unwrap();
    reset_test_fs();
    let _code = BuddyHome::new("CODEBUDDY_CONFIG_DIR");
    let work = BuddyHome::new("WORKBUDDY_CONFIG_DIR");
    let state = create_test_state().unwrap();
    fs::write(
        work.models_json(),
        json!({"models": [{"id": "kimi-k3", "name": "Kimi", "vendor": "Restored",
                           "url": "https://restored.example/v1/chat/completions",
                           "apiKey": "test-value", "ccSwitchProvider": "restored"}]})
        .to_string(),
    )
    .unwrap();
    let providers = ProviderService::list(&state, AppType::CodeBuddy).unwrap();
    let restored = &providers["restored"];
    assert_eq!(restored.name, "Restored");
    assert_eq!(
        restored.settings_config["options"]["baseURL"],
        "https://restored.example/v1"
    );
    assert_eq!(managed(&state, "restored"), Some(true));
}
