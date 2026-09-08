use super::*;

#[tokio::test]
async fn demo_and_logo_are_public_fixed_assets_with_exact_bytes() {
    let service = embedded_service();
    for (path, expected, mime) in [
        ("/demo", DEMO_HTML.as_bytes(), "text/html; charset=utf-8"),
        ("/webcodex-logo.png", BRAND_LOGO_PNG, "image/png"),
    ] {
        let mut response = TestClient::get(format!("http://localhost{path}"))
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(header(&response, "content-type"), mime);
        assert_eq!(header(&response, "cache-control"), EMBEDDED_CACHE_CONTROL);
        assert_eq!(response.take_bytes(None).await.unwrap().as_ref(), expected);
    }
    // The public tour grants no access to application facts or execution.
    let response = TestClient::post("http://localhost/api/connector/readiness")
        .json(&serde_json::json!({}))
        .send(&service)
        .await;
    assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
}

#[tokio::test]
async fn development_demo_and_binary_logo_reload_and_fail_closed_when_missing() {
    let temp = tempfile::tempdir().unwrap();
    write_development_assets(temp.path());
    let source = ConsoleAssetSource::from_directory(temp.path()).unwrap();
    for (asset, bytes) in [
        (ConsoleAsset::DemoHtml, b"<html>demo</html>".as_slice()),
        (
            ConsoleAsset::BrandLogo,
            b"\x89PNG\r\n\x1a\n\xff\x00".as_slice(),
        ),
    ] {
        assert!(source.read(asset).await.is_err());
        let path = temp.path().join(asset.file_name());
        fs::write(&path, bytes).unwrap();
        assert_eq!(source.read(asset).await.unwrap(), bytes);
        fs::remove_file(path).unwrap();
        assert!(source.read(asset).await.is_err());
    }
    // Adding binary delivery must not relax text-bundle UTF-8 validation.
    fs::write(temp.path().join("demo.html"), [0xff]).unwrap();
    assert!(source.read(ConsoleAsset::DemoHtml).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn demo_and_logo_reject_development_symlink_substitution() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write_development_assets(temp.path());
    let source = ConsoleAssetSource::from_directory(temp.path()).unwrap();
    let private_file = outside.path().join("private");
    fs::write(&private_file, "must not be served").unwrap();
    for asset in [ConsoleAsset::DemoHtml, ConsoleAsset::BrandLogo] {
        symlink(&private_file, temp.path().join(asset.file_name())).unwrap();
        assert!(source.read(asset).await.is_err());
    }
}
