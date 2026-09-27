use ai_fence_cli_core::agent_launcher::{write_codex_config, CodexProviderAuth};
use std::fs;

// Configuration generation must never manufacture a login or overwrite one.
// These tests do not launch Codex, execute an auth helper, or contact a service.
#[test]
fn provider_configuration_does_not_create_or_replace_native_login() {
    for auth in [
        CodexProviderAuth::EnvKey,
        CodexProviderAuth::EnvBearer {
            env_key: "PROVIDER_TOKEN".to_string(),
        },
        CodexProviderAuth::Command {
            command: "never-execute-this-auth-helper".to_string(),
            args: vec!["--auth-json-env".to_string(), "RENTAL_PATH".to_string()],
        },
        CodexProviderAuth::OpenAiAuth,
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("profile");
        let native_auth = home.join("auth.json");
        for existing_login in [false, true] {
            if existing_login {
                fs::write(&native_auth, b"native-login-sentinel").expect("seed native auth");
            }
            write_codex_config(
                &home,
                "http://127.0.0.1:1234",
                None,
                None,
                false,
                auth.clone(),
            )
            .expect("write provider config");
            if existing_login {
                assert_eq!(fs::read(&native_auth).unwrap(), b"native-login-sentinel");
            } else {
                assert!(
                    !native_auth.exists(),
                    "provider config must not create login"
                );
            }
        }
    }
}

#[test]
fn pooled_provider_replaces_stale_native_auth_without_changing_account_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let template = temp.path().join("template");
    fs::create_dir(&template).expect("template dir");
    fs::write(
        template.join("codex-config.toml"),
        r#"
[model_providers.ai_fence]
requires_openai_auth = true
env_key = "STALE_PERSONAL_KEY"
"#,
    )
    .expect("template");
    let home = temp.path().join("profile");
    write_codex_config(
        &home,
        "http://127.0.0.1:1234",
        None,
        Some(&template),
        false,
        CodexProviderAuth::Command {
            command: "never-execute-this-auth-helper".to_string(),
            args: vec!["--auth-json-env".to_string(), "RENTAL_PATH".to_string()],
        },
    )
    .expect("write pooled config");
    let config: toml::Value = fs::read_to_string(home.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let provider = config["model_providers"]["ai_fence"].as_table().unwrap();
    assert!(!provider.contains_key("requires_openai_auth"));
    assert!(!provider.contains_key("env_key"));
    assert_eq!(
        provider["auth"]["command"].as_str(),
        Some("never-execute-this-auth-helper")
    );
    assert_eq!(provider["auth"]["args"][1].as_str(), Some("RENTAL_PATH"));
    assert!(!home.join("auth.json").exists());
}
