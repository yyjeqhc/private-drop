use super::*;
use crate::tool_runtime::permissions::{AuthorityMode, PermissionEvaluator};

#[tokio::test]
async fn http_runner_reload_enforces_restricted_authority() {
    let _env = crate::auth::AuthEnvGuard::auth_required();
    let root = tempfile::tempdir().unwrap();
    let (runtime, registry) = register_import_agent_with_capabilities(
        root.path(),
        Some(crate::runner_protocol::RunnerCapabilities {
            runner_config_control: true,
            ..Default::default()
        }),
    )
    .await;
    let runtime = Arc::new(
        Arc::try_unwrap(runtime)
            .ok()
            .unwrap()
            .with_permission_evaluator(PermissionEvaluator::with_mode(AuthorityMode::Restricted)),
    );
    let (_tmp, db) = test_db();
    let service = Service::new(
        Router::new()
            .hoop(affix_state::inject(test_config(Some("secret"))))
            .hoop(affix_state::inject(db))
            .hoop(affix_state::inject(runtime))
            .hoop(crate::AuthMiddleware)
            .push(Router::with_path("api/runners/config/reload").post(runner_config_reload)),
    );
    let mut response = TestClient::post("http://localhost/api/runners/config/reload")
        .bearer_auth("secret")
        .json(&json!({"client_id":"importer","expected_generation":1}))
        .send(&service)
        .await;
    let body: Value = response.take_json().await.unwrap();
    assert_eq!(body["success"], false);
    assert_eq!(body["output"]["permission"]["status"], "denied");
    assert_eq!(
        body["output"]["permission"]["reason"],
        "restricted_requires_human_authorization"
    );
    assert!(registry
        .poll(crate::runner_protocol::RunnerPollRequest {
            client_id: "importer".into(),
            runner_instance_id: "inst-import".into(),
        })
        .await
        .unwrap()
        .is_none());
}
