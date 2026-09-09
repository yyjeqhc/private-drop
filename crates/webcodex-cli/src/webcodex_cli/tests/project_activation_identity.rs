use super::tests::{activation_config, spawn_operator_server_with_hook};
use super::*;
use crate::webcodex_cli::test_support::canonical_test_tempdir;

#[tokio::test]
async fn activation_stops_if_connection_identity_changes_during_config_retry() {
    // Exercise both retry entry points and both parts of the connection identity.
    for generation_conflict in [false, true] {
        for replace_server in [false, true] {
            let tmp = canonical_test_tempdir();
            let project = tmp.path().join("demo");
            std::fs::create_dir(&project).unwrap();
            let config_path = tmp.path().join("runner.toml");
            let mut responses = vec![(
                "/api/runners/config/check",
                json!({"success":true,"output":{"valid":true,"current_generation":1,"restart_required":false}}),
            )];
            if generation_conflict {
                responses.push((
                    "/api/runners/config/reload",
                    json!({"success":false,"output":{"error_code":"config_generation_conflict"}}),
                ));
            }
            let replacement_at = responses.len();
            let edited_path = config_path.clone();
            let (server_url, server) = spawn_operator_server_with_hook(responses, move |index| {
                if index == replacement_at {
                    let current = std::fs::read_to_string(&edited_path).unwrap();
                    let mut replacement = current.parse::<toml_edit::DocumentMut>().unwrap();
                    if replace_server {
                        replacement["server_url"] = toml_edit::value("https://replacement.test");
                    } else {
                        replacement["client_id"] = toml_edit::value("replacement-client");
                    }
                    replacement["policy"]["allowed_roots"] =
                        toml_edit::value(toml_edit::Array::new());
                    std::fs::write(&edited_path, replacement.to_string()).unwrap();
                }
            });
            activation_config(&config_path, &server_url, &[], false);
            let token_file = tmp.path().join("user-token");
            std::fs::write(&token_file, "wc_pat_project_activation_test").unwrap();

            let error = run_project_activate(ProjectActivateOptions {
                config: config_path.clone(),
                user_token_file: token_file,
                project,
                json: true,
            })
            .await
            .unwrap_err();
            server.join().unwrap();
            assert!(
                error.starts_with("project_activation_config_conflict:"),
                "{error}"
            );
            let saved: toml::Value =
                toml::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
            assert!(
                saved["policy"]["allowed_roots"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
                "an old activation must not extend the replacement connection's authority"
            );
        }
    }
}
