use super::super::support::*;

#[test]
fn server_tunnel_parser_is_machine_owned_and_openai_only() {
    let parsed = parse_server_tunnel(&args(&[
        "--provider",
        "openai",
        "--env-file",
        "local.env",
        "--json",
        "--stop-on-stdin-eof",
    ]))
    .unwrap();
    assert_eq!(parsed.env_file, PathBuf::from("local.env"));

    assert!(parse_server_tunnel(&args(&[
        "--provider",
        "cloudflare",
        "--env-file",
        "local.env",
        "--json",
        "--stop-on-stdin-eof",
    ]))
    .unwrap_err()
    .contains("openai"));
    assert!(
        parse_server_tunnel(&args(&["--provider", "openai", "--env-file", "local.env",])).is_err()
    );
}

#[test]
fn regular_tunnel_bootstrap_token_follows_server_env_precedence() {
    let _guard = env_test_guard();
    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("webcodex.env");
    std::fs::write(&env_file, "WEBCODEX_TOKEN=file-bootstrap\n").unwrap();

    let _env = EnvGuard::new().remove("WEBCODEX_TOKEN");
    assert_eq!(
        crate::webcodex_cli::server::derive_regular_tunnel_bootstrap_token(&env_file).unwrap(),
        "file-bootstrap"
    );
    drop(_env);

    let _env = EnvGuard::new().set("WEBCODEX_TOKEN", "process-bootstrap");
    assert_eq!(
        crate::webcodex_cli::server::derive_regular_tunnel_bootstrap_token(&env_file).unwrap(),
        "process-bootstrap"
    );
}

#[test]
fn regular_tunnel_server_url_is_derived_from_loopback_env_only() {
    let tmp = tempfile::tempdir().unwrap();
    let env_file = tmp.path().join("webcodex.env");
    std::fs::write(&env_file, "WEBCODEX_ADDR=0.0.0.0:18080\n").unwrap();
    assert_eq!(
        crate::webcodex_cli::server::derive_regular_tunnel_server_url(&env_file).unwrap(),
        "http://127.0.0.1:18080"
    );

    std::fs::write(&env_file, "WEBCODEX_ADDR=192.0.2.10:18080\n").unwrap();
    assert!(crate::webcodex_cli::server::derive_regular_tunnel_server_url(&env_file).is_err());
}
