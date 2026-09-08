//! Minimal project-readiness console.
//!
//! Serves the public static shells for the project Review Console, hosted
//! Runtime Console, and Admin. The bundles contain no credentials; each browser
//! authenticates separately for its protected API requests.
//!
//! Production uses the committed `frontend/dist/` bundle embedded at compile
//! time. Development mode validates the core Project Review Console files once
//! at startup; every requested fixed Review/Admin/Runtime asset is then
//! path-checked and re-read from that same loopback-only directory.

use salvo::http::header::{CACHE_CONTROL, CONTENT_TYPE, PRAGMA};
use salvo::http::{HeaderName, HeaderValue};
use salvo::prelude::*;
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const CONSOLE_ASSETS_DIR_ENV: &str = "WEBCODEX_CONSOLE_ASSETS_DIR";

const ASSET_MODE_HEADER: HeaderName = HeaderName::from_static("x-webcodex-console-assets");
const EMBEDDED_CACHE_CONTROL: &str = "no-cache, must-revalidate";
const DEVELOPMENT_CACHE_CONTROL: &str = "no-store";
const DEVELOPMENT_ERROR_BODY: &str = "Console development asset is unavailable.\n";

// The committed build stays embedded so production has no runtime filesystem
// dependency.
const CONSOLE_HTML: &str = include_str!("../frontend/dist/console.html");
const CONSOLE_APP_JS: &str = include_str!("../frontend/dist/app.js");
const CONSOLE_STYLES_CSS: &str = include_str!("../frontend/dist/styles.css");
const ADMIN_HTML: &str = include_str!("../frontend/dist/admin.html");
const ADMIN_APP_JS: &str = include_str!("../frontend/dist/admin.js");
const ADMIN_STYLES_CSS: &str = include_str!("../frontend/dist/admin.css");
const RUNTIME_HTML: &str = include_str!("../frontend/dist/runtime.html");
const RUNTIME_APP_JS: &str = include_str!("../frontend/dist/runtime.js");
const RUNTIME_STYLES_CSS: &str = include_str!("../frontend/dist/runtime.css");
const DEMO_HTML: &str = include_str!("../frontend/dist/demo.html");
const BRAND_LOGO_PNG: &[u8] = include_bytes!("../frontend/dist/webcodex-logo.png");

#[derive(Debug, Clone, Copy)]
enum ConsoleAsset {
    Html,
    JavaScript,
    Css,
    AdminHtml,
    AdminJavaScript,
    AdminCss,
    RuntimeHtml,
    RuntimeJavaScript,
    RuntimeCss,
    DemoHtml,
    BrandLogo,
}

impl ConsoleAsset {
    const fn file_name(self) -> &'static str {
        match self {
            Self::Html => "console.html",
            Self::JavaScript => "app.js",
            Self::Css => "styles.css",
            Self::AdminHtml => "admin.html",
            Self::AdminJavaScript => "admin.js",
            Self::AdminCss => "admin.css",
            Self::RuntimeHtml => "runtime.html",
            Self::RuntimeJavaScript => "runtime.js",
            Self::RuntimeCss => "runtime.css",
            Self::DemoHtml => "demo.html",
            Self::BrandLogo => "webcodex-logo.png",
        }
    }

    const fn content_type(self) -> &'static str {
        match self {
            Self::Html => "text/html; charset=utf-8",
            Self::JavaScript => "application/javascript; charset=utf-8",
            Self::Css | Self::AdminCss | Self::RuntimeCss => "text/css; charset=utf-8",
            Self::AdminHtml | Self::RuntimeHtml | Self::DemoHtml => "text/html; charset=utf-8",
            Self::BrandLogo => "image/png",
            Self::AdminJavaScript | Self::RuntimeJavaScript => {
                "application/javascript; charset=utf-8"
            }
        }
    }

    const fn embedded(self) -> &'static [u8] {
        match self {
            Self::Html => CONSOLE_HTML.as_bytes(),
            Self::JavaScript => CONSOLE_APP_JS.as_bytes(),
            Self::Css => CONSOLE_STYLES_CSS.as_bytes(),
            Self::AdminHtml => ADMIN_HTML.as_bytes(),
            Self::AdminJavaScript => ADMIN_APP_JS.as_bytes(),
            Self::AdminCss => ADMIN_STYLES_CSS.as_bytes(),
            Self::RuntimeHtml => RUNTIME_HTML.as_bytes(),
            Self::RuntimeJavaScript => RUNTIME_APP_JS.as_bytes(),
            Self::RuntimeCss => RUNTIME_STYLES_CSS.as_bytes(),
            Self::DemoHtml => DEMO_HTML.as_bytes(),
            Self::BrandLogo => BRAND_LOGO_PNG,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ConsoleAssetDirectory {
    canonical_root: PathBuf,
}

/// The single startup-resolved source shared by all three console handlers.
#[derive(Debug, Clone, Default)]
pub(crate) enum ConsoleAssetSource {
    #[default]
    Embedded,
    Directory(ConsoleAssetDirectory),
}

#[derive(Debug)]
pub(crate) struct ConsoleAssetConfigError {
    message: String,
}

impl ConsoleAssetConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConsoleAssetConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConsoleAssetConfigError {}

impl ConsoleAssetSource {
    /// Resolve the private serve-process environment once during startup.
    pub(crate) fn from_env(bind_addr: &str) -> Result<Self, ConsoleAssetConfigError> {
        let Some(directory) = std::env::var_os(CONSOLE_ASSETS_DIR_ENV) else {
            return Ok(Self::Embedded);
        };
        Self::from_directory_for_addr(PathBuf::from(directory), bind_addr)
    }

    /// Validate a development source for a specific HTTP bind address.
    pub(crate) fn from_directory_for_addr(
        directory: impl AsRef<Path>,
        bind_addr: &str,
    ) -> Result<Self, ConsoleAssetConfigError> {
        validate_loopback_addr(bind_addr)?;
        Self::from_directory(directory)
    }

    /// Canonicalize the development root and require the three Project Review
    /// Console core files. Admin and Runtime assets remain optional in this
    /// compatibility mode and are validated fail-closed when requested.
    pub(crate) fn from_directory(
        directory: impl AsRef<Path>,
    ) -> Result<Self, ConsoleAssetConfigError> {
        let directory = directory.as_ref();
        if !directory.is_absolute() {
            return Err(ConsoleAssetConfigError::new(
                "console development assets directory must be an absolute path",
            ));
        }
        let canonical_root = fs::canonicalize(directory).map_err(|_| {
            ConsoleAssetConfigError::new(
                "console development assets directory does not exist or is inaccessible",
            )
        })?;
        if !fs::metadata(&canonical_root)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            return Err(ConsoleAssetConfigError::new(
                "console development assets path is not a directory",
            ));
        }

        let source = Self::Directory(ConsoleAssetDirectory { canonical_root });
        for asset in [
            ConsoleAsset::Html,
            ConsoleAsset::JavaScript,
            ConsoleAsset::Css,
        ] {
            let path = source.validated_path(asset)?;
            fs::File::open(path).map_err(|_| {
                ConsoleAssetConfigError::new(format!(
                    "console development asset {} is not readable",
                    asset.file_name()
                ))
            })?;
        }
        Ok(source)
    }

    pub(crate) fn mode_label(&self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Directory(_) => "local development",
        }
    }

    pub(crate) fn directory(&self) -> Option<&Path> {
        match self {
            Self::Embedded => None,
            Self::Directory(directory) => Some(&directory.canonical_root),
        }
    }

    fn diagnostic_label(&self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Directory(_) => "filesystem",
        }
    }

    fn is_development(&self) -> bool {
        matches!(self, Self::Directory(_))
    }

    fn validated_path(&self, asset: ConsoleAsset) -> Result<PathBuf, ConsoleAssetConfigError> {
        let Self::Directory(directory) = self else {
            return Err(ConsoleAssetConfigError::new(
                "embedded console assets do not have filesystem paths",
            ));
        };
        let candidate = directory.canonical_root.join(asset.file_name());

        // Files are fixed by the server, never by URL input. Re-checking the
        // symlink and canonical-path constraints rejects persistent path
        // substitution. A process with concurrent write access to this directory
        // is already trusted to control the development bundle itself.
        let link_metadata = fs::symlink_metadata(&candidate).map_err(|_| {
            ConsoleAssetConfigError::new(format!(
                "console development asset {} is unavailable",
                asset.file_name()
            ))
        })?;
        if link_metadata.file_type().is_symlink() {
            return Err(ConsoleAssetConfigError::new(format!(
                "console development asset {} must not be a symbolic link",
                asset.file_name()
            )));
        }
        let canonical = fs::canonicalize(&candidate).map_err(|_| {
            ConsoleAssetConfigError::new(format!(
                "console development asset {} is unavailable",
                asset.file_name()
            ))
        })?;
        if !canonical.starts_with(&directory.canonical_root) {
            return Err(ConsoleAssetConfigError::new(format!(
                "console development asset {} escapes its configured directory",
                asset.file_name()
            )));
        }
        if !fs::metadata(&canonical)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            return Err(ConsoleAssetConfigError::new(format!(
                "console development asset {} is not a regular file",
                asset.file_name()
            )));
        }
        Ok(canonical)
    }

    async fn read(&self, asset: ConsoleAsset) -> Result<Vec<u8>, ConsoleAssetConfigError> {
        match self {
            Self::Embedded => Ok(asset.embedded().to_vec()),
            Self::Directory(_) => {
                let path = self.validated_path(asset)?;
                // Keep UTF-8 validation for text bundles; only the fixed logo is binary.
                let body = if matches!(asset, ConsoleAsset::BrandLogo) {
                    tokio::fs::read(path).await
                } else {
                    tokio::fs::read_to_string(path)
                        .await
                        .map(String::into_bytes)
                };
                body.map_err(|_| {
                    ConsoleAssetConfigError::new(format!(
                        "console development asset {} could not be read",
                        asset.file_name()
                    ))
                })
            }
        }
    }
}

fn validate_loopback_addr(bind_addr: &str) -> Result<(), ConsoleAssetConfigError> {
    let is_loopback = bind_addr
        .parse::<SocketAddr>()
        .map(|address| address.ip().is_loopback())
        .unwrap_or_else(|_| {
            bind_addr
                .strip_prefix("localhost:")
                .and_then(|port| port.parse::<u16>().ok())
                .is_some()
        });
    if is_loopback {
        Ok(())
    } else {
        Err(ConsoleAssetConfigError::new(format!(
            "{CONSOLE_ASSETS_DIR_ENV} requires WEBCODEX_ADDR to use a loopback address"
        )))
    }
}

fn apply_asset_headers(res: &mut Response, source: &ConsoleAssetSource, asset: ConsoleAsset) {
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(asset.content_type()));
    res.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if source.is_development() {
            DEVELOPMENT_CACHE_CONTROL
        } else {
            EMBEDDED_CACHE_CONTROL
        }),
    );
    if source.is_development() {
        res.headers_mut()
            .insert(PRAGMA, HeaderValue::from_static("no-cache"));
    }
    res.headers_mut().insert(
        ASSET_MODE_HEADER,
        HeaderValue::from_static(source.diagnostic_label()),
    );
}

async fn serve_asset(depot: &Depot, res: &mut Response, asset: ConsoleAsset) {
    let Ok(source) = depot.obtain::<Arc<ConsoleAssetSource>>() else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Text::Plain("Console asset source is unavailable.\n"));
        return;
    };
    apply_asset_headers(res, source, asset);
    match source.read(asset).await {
        Ok(body) => {
            res.body(salvo::http::ResBody::Once(body.into()));
        }
        Err(error) => {
            tracing::error!(
                asset = asset.file_name(),
                error = %error,
                "Console development asset request failed"
            );
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Text::Plain(DEVELOPMENT_ERROR_BODY));
        }
    }
}

/// `GET /console` — the console HTML shell. Public.
#[handler]
pub async fn console_html(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::Html).await;
}

/// `GET /console/app.js` — the console application script. Public.
#[handler]
pub async fn console_app_js(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::JavaScript).await;
}

/// `GET /console/styles.css` — the console stylesheet. Public.
#[handler]
pub async fn console_styles_css(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::Css).await;
}

/// Public hosted Runtime Console shell; all data stays behind
/// `/api/runtime-console/*` and ordinary runtime authorization.
#[handler]
pub async fn runtime_html(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::RuntimeHtml).await;
}

#[handler]
pub async fn runtime_app_js(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::RuntimeJavaScript).await;
}

#[handler]
pub async fn runtime_styles_css(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::RuntimeCss).await;
}

/// Public, static feature tour. Its fictional state is confined to page memory.
#[handler]
pub async fn demo_html(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::DemoHtml).await;
}

#[handler]
pub async fn brand_logo_png(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::BrandLogo).await;
}

/// Public admin shell; all data remains protected by `/api/admin/*`.
#[handler]
pub async fn admin_html(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::AdminHtml).await;
}

#[handler]
pub async fn admin_app_js(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::AdminJavaScript).await;
}

#[handler]
pub async fn admin_styles_css(depot: &Depot, res: &mut Response) {
    serve_asset(depot, res, ConsoleAsset::AdminCss).await;
}

#[cfg(test)]
mod tests {
    mod demo_assets;

    use super::*;
    use crate::test_support::{test_config, test_db};
    use salvo::test::{ResponseExt, TestClient};
    use salvo::Service;

    fn build_test_router(
        config: Arc<crate::Config>,
        db: Arc<crate::Database>,
        source: Arc<ConsoleAssetSource>,
    ) -> Router {
        let runner_registry = Arc::new(crate::RunnerRegistry::default());
        let runtime_info = Arc::new(crate::tool_runtime::RuntimeInfo::default());
        let tool_runtime = Arc::new(crate::tool_runtime::ToolRuntime::new(
            runner_registry,
            runtime_info,
        ));
        Router::new()
            .hoop(affix_state::inject(config))
            .hoop(affix_state::inject(db))
            .hoop(affix_state::inject(tool_runtime))
            .hoop(affix_state::inject(source))
            .hoop(affix_state::inject(
                crate::connector_runtime::ConnectorRuntimeSlot::default(),
            ))
            .push(Router::with_path("console").get(console_html))
            .push(Router::with_path("console/app.js").get(console_app_js))
            .push(Router::with_path("console/styles.css").get(console_styles_css))
            .push(Router::with_path("runtime").get(runtime_html))
            .push(Router::with_path("runtime/app.js").get(runtime_app_js))
            .push(Router::with_path("runtime/styles.css").get(runtime_styles_css))
            .push(Router::with_path("demo").get(demo_html))
            .push(Router::with_path("webcodex-logo.png").get(brand_logo_png))
            .push(Router::with_path("admin").get(admin_html))
            .push(Router::with_path("admin/app.js").get(admin_app_js))
            .push(Router::with_path("admin/styles.css").get(admin_styles_css))
            .push(
                Router::with_path("api")
                    .hoop(crate::AuthMiddleware)
                    .push(crate::connector_runtime::http::routes()),
            )
    }

    fn embedded_service() -> Service {
        let config = test_config(Some("secret"));
        let (_tmp, db) = test_db();
        Service::new(build_test_router(
            config,
            db,
            Arc::new(ConsoleAssetSource::Embedded),
        ))
    }

    fn header(resp: &Response, name: &str) -> String {
        resp.headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    fn write_development_assets(directory: &Path) {
        fs::create_dir_all(directory).unwrap();
        fs::write(
            directory.join("console.html"),
            "<html>filesystem html</html>\n",
        )
        .unwrap();
        fs::write(directory.join("app.js"), "globalThis.filesystemJs = 1;\n").unwrap();
        fs::write(
            directory.join("styles.css"),
            ".filesystem { color: red; }\n",
        )
        .unwrap();
    }

    fn development_service(directory: &Path) -> Service {
        let config = test_config(Some("secret"));
        let (_tmp, db) = test_db();
        let source =
            ConsoleAssetSource::from_directory_for_addr(directory, "127.0.0.1:8080").unwrap();
        Service::new(build_test_router(config, db, Arc::new(source)))
    }

    #[test]
    fn embedded_bundle_is_non_empty_and_has_expected_markers() {
        assert!(!CONSOLE_HTML.is_empty());
        assert!(CONSOLE_HTML.contains("/console/app.js"));
        assert!(CONSOLE_HTML.contains("/console/styles.css"));
        assert!(CONSOLE_APP_JS.contains("/api/console/"));
        assert!(!CONSOLE_APP_JS.contains("/api/runtime/status"));
        assert!(!CONSOLE_APP_JS.contains("localStorage"));
        assert!(!CONSOLE_APP_JS.contains("sessionStorage"));
        assert!(!CONSOLE_APP_JS.contains(".innerHTML"));
        assert!(CONSOLE_APP_JS.contains("performAction"));
        assert!(CONSOLE_STYLES_CSS.contains("[hidden]{display:none !important}"));
        assert!(CONSOLE_HTML.contains("type=\"password\""));
        assert!(CONSOLE_HTML.contains("WebCodex — Project Review Console"));
        assert!(!RUNTIME_HTML.is_empty());
        assert!(RUNTIME_HTML.contains("WebCodex Runtime Console"));
        assert!(RUNTIME_HTML.contains("/runtime/app.js"));
        assert!(RUNTIME_HTML.contains("/runtime/styles.css"));
        assert!(RUNTIME_APP_JS.contains("/api/runtime-console/"));
        assert!(!RUNTIME_APP_JS.contains("/api/console/"));
        // Runtime UI preferences may use localStorage, while tab-scoped drafts and the
        // optional remembered Bearer credential use sessionStorage. Credentials must
        // never move into durable localStorage or cookies.
        assert!(RUNTIME_APP_JS.contains("localStorage"));
        assert!(RUNTIME_APP_JS.contains("sessionStorage"));
        assert!(RUNTIME_APP_JS.contains("RUNTIME_CREDENTIAL_SESSION_KEY"));
        assert!(RUNTIME_APP_JS.contains("sessionStorage.setItem(RUNTIME_CREDENTIAL_SESSION_KEY"));
        assert!(!RUNTIME_APP_JS.contains("localStorage.setItem(RUNTIME_CREDENTIAL_SESSION_KEY"));
        assert!(!RUNTIME_APP_JS.contains("document.cookie"));
        assert!(!RUNTIME_APP_JS.contains(".innerHTML"));
        assert!(!CONSOLE_HTML.contains("Transport"));
    }

    #[tokio::test]
    async fn embedded_http_assets_preserve_bodies_mime_and_cache_policy() {
        let service = embedded_service();
        for (url, expected, mime) in [
            ("http://localhost/console", CONSOLE_HTML, "text/html"),
            (
                "http://localhost/console/app.js",
                CONSOLE_APP_JS,
                "application/javascript",
            ),
            (
                "http://localhost/console/styles.css",
                CONSOLE_STYLES_CSS,
                "text/css",
            ),
        ] {
            let mut resp = TestClient::get(url).send(&service).await;
            assert_eq!(resp.status_code, Some(StatusCode::OK));
            assert!(header(&resp, "content-type").contains(mime));
            assert_eq!(header(&resp, "cache-control"), "no-cache, must-revalidate");
            assert_eq!(header(&resp, "x-webcodex-console-assets"), "embedded");
            assert_eq!(resp.take_string().await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn embedded_runtime_assets_preserve_bodies_mime_and_cache_policy() {
        let service = embedded_service();
        for (url, expected, mime) in [
            ("http://localhost/runtime", RUNTIME_HTML, "text/html"),
            (
                "http://localhost/runtime/app.js",
                RUNTIME_APP_JS,
                "application/javascript",
            ),
            (
                "http://localhost/runtime/styles.css",
                RUNTIME_STYLES_CSS,
                "text/css",
            ),
        ] {
            let mut resp = TestClient::get(url).send(&service).await;
            assert_eq!(resp.status_code, Some(StatusCode::OK));
            assert!(header(&resp, "content-type").contains(mime));
            assert_eq!(header(&resp, "cache-control"), "no-cache, must-revalidate");
            assert_eq!(resp.take_string().await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn filesystem_http_assets_preserve_bodies_mime_and_no_store_headers() {
        let temp = tempfile::tempdir().unwrap();
        write_development_assets(temp.path());
        let service = development_service(temp.path());
        for (url, expected, mime) in [
            (
                "http://localhost/console",
                "<html>filesystem html</html>\n",
                "text/html",
            ),
            (
                "http://localhost/console/app.js",
                "globalThis.filesystemJs = 1;\n",
                "application/javascript",
            ),
            (
                "http://localhost/console/styles.css",
                ".filesystem { color: red; }\n",
                "text/css",
            ),
        ] {
            let mut resp = TestClient::get(url).send(&service).await;
            assert_eq!(resp.status_code, Some(StatusCode::OK));
            assert!(header(&resp, "content-type").contains(mime));
            assert_eq!(header(&resp, "cache-control"), "no-store");
            assert_eq!(header(&resp, "pragma"), "no-cache");
            assert_eq!(header(&resp, "x-webcodex-console-assets"), "filesystem");
            assert_eq!(resp.take_string().await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn filesystem_js_is_reread_and_missing_file_never_falls_back() {
        let temp = tempfile::tempdir().unwrap();
        write_development_assets(temp.path());
        let service = development_service(temp.path());

        fs::write(temp.path().join("app.js"), "globalThis.filesystemJs = 2;\n").unwrap();
        let mut updated = TestClient::get("http://localhost/console/app.js")
            .send(&service)
            .await;
        assert_eq!(updated.status_code, Some(StatusCode::OK));
        assert_eq!(
            updated.take_string().await.unwrap(),
            "globalThis.filesystemJs = 2;\n"
        );

        fs::remove_file(temp.path().join("app.js")).unwrap();
        let mut missing = TestClient::get("http://localhost/console/app.js")
            .send(&service)
            .await;
        assert_eq!(missing.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
        assert_eq!(header(&missing, "cache-control"), "no-store");
        assert_eq!(header(&missing, "x-webcodex-console-assets"), "filesystem");
        let body = missing.take_string().await.unwrap();
        assert_eq!(body, DEVELOPMENT_ERROR_BODY);
        assert!(!body.contains("performAction"));
    }

    #[tokio::test]
    async fn admin_assets_cover_embedded_and_filesystem_modes() {
        let embedded = embedded_service();
        for (url, expected, mime) in [
            ("http://localhost/admin", ADMIN_HTML, "text/html"),
            (
                "http://localhost/admin/app.js",
                ADMIN_APP_JS,
                "application/javascript",
            ),
            (
                "http://localhost/admin/styles.css",
                ADMIN_STYLES_CSS,
                "text/css",
            ),
        ] {
            let mut response = TestClient::get(url).send(&embedded).await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            assert!(header(&response, "content-type").contains(mime));
            assert_eq!(
                header(&response, "cache-control"),
                "no-cache, must-revalidate"
            );
            assert_eq!(response.take_string().await.unwrap(), expected);
        }

        let temp = tempfile::tempdir().unwrap();
        write_development_assets(temp.path());
        fs::write(temp.path().join("admin.html"), "<html>admin one</html>\n").unwrap();
        fs::write(temp.path().join("admin.js"), "globalThis.admin = 1;\n").unwrap();
        fs::write(temp.path().join("admin.css"), ".admin { color: blue; }\n").unwrap();
        let development = development_service(temp.path());
        for (url, expected, mime) in [
            (
                "http://localhost/admin",
                "<html>admin one</html>\n",
                "text/html",
            ),
            (
                "http://localhost/admin/app.js",
                "globalThis.admin = 1;\n",
                "application/javascript",
            ),
            (
                "http://localhost/admin/styles.css",
                ".admin { color: blue; }\n",
                "text/css",
            ),
        ] {
            let mut response = TestClient::get(url).send(&development).await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            assert!(header(&response, "content-type").contains(mime));
            assert_eq!(header(&response, "cache-control"), "no-store");
            assert_eq!(header(&response, "pragma"), "no-cache");
            assert_eq!(response.take_string().await.unwrap(), expected);
        }

        fs::write(temp.path().join("admin.html"), "<html>admin two</html>\n").unwrap();
        let mut updated = TestClient::get("http://localhost/admin")
            .send(&development)
            .await;
        assert_eq!(
            updated.take_string().await.unwrap(),
            "<html>admin two</html>\n"
        );
        fs::remove_file(temp.path().join("admin.html")).unwrap();
        let mut missing = TestClient::get("http://localhost/admin")
            .send(&development)
            .await;
        assert_eq!(missing.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
        assert_eq!(header(&missing, "cache-control"), "no-store");
        assert_eq!(missing.take_string().await.unwrap(), DEVELOPMENT_ERROR_BODY);

        let mut console = TestClient::get("http://localhost/console")
            .send(&development)
            .await;
        assert_eq!(console.status_code, Some(StatusCode::OK));
        assert_eq!(
            console.take_string().await.unwrap(),
            "<html>filesystem html</html>\n"
        );
    }

    #[tokio::test]
    async fn console_only_development_directory_keeps_console_available() {
        let temp = tempfile::tempdir().unwrap();
        write_development_assets(temp.path());
        let service = development_service(temp.path());
        let console = TestClient::get("http://localhost/console")
            .send(&service)
            .await;
        assert_eq!(console.status_code, Some(StatusCode::OK));
        let mut admin = TestClient::get("http://localhost/admin")
            .send(&service)
            .await;
        assert_eq!(admin.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
        assert_eq!(header(&admin, "cache-control"), "no-store");
        assert_eq!(admin.take_string().await.unwrap(), DEVELOPMENT_ERROR_BODY);
    }

    #[test]
    fn development_directory_rejects_relative_missing_and_incomplete_paths() {
        let relative = ConsoleAssetSource::from_directory("frontend/.dev-dist").unwrap_err();
        assert!(relative.to_string().contains("absolute"));

        let temp = tempfile::tempdir().unwrap();
        let missing = ConsoleAssetSource::from_directory(temp.path().join("missing")).unwrap_err();
        assert!(missing.to_string().contains("does not exist"));

        fs::write(temp.path().join("console.html"), "html").unwrap();
        let incomplete = ConsoleAssetSource::from_directory(temp.path()).unwrap_err();
        assert!(incomplete.to_string().contains("app.js"));
    }

    #[cfg(unix)]
    #[test]
    fn development_directory_rejects_file_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let assets = temp.path().join("assets");
        write_development_assets(&assets);
        let outside = temp.path().join("outside.js");
        fs::write(&outside, "globalThis.outside = true;\n").unwrap();
        fs::remove_file(assets.join("app.js")).unwrap();
        symlink(outside, assets.join("app.js")).unwrap();

        let error = ConsoleAssetSource::from_directory(&assets).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
    }

    #[test]
    fn development_assets_require_loopback_bind_address() {
        let temp = tempfile::tempdir().unwrap();
        write_development_assets(temp.path());
        for allowed in ["127.0.0.1:8080", "[::1]:8080", "localhost:8080"] {
            ConsoleAssetSource::from_directory_for_addr(temp.path(), allowed).unwrap();
        }
        for denied in ["0.0.0.0:8080", "[::]:8080", "192.0.2.10:8080"] {
            let error =
                ConsoleAssetSource::from_directory_for_addr(temp.path(), denied).unwrap_err();
            assert!(error.to_string().contains("loopback"));
        }
    }

    #[tokio::test]
    async fn http_readiness_requires_bearer_auth() {
        let _env = crate::auth::AuthEnvGuard::auth_required();
        let service = embedded_service();
        let resp = TestClient::post("http://localhost/api/connector/readiness")
            .json(&serde_json::json!({}))
            .send(&service)
            .await;
        assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    }

    #[tokio::test]
    async fn http_readiness_projects_setup_action_with_bearer_auth() {
        let service = embedded_service();
        let mut resp = TestClient::post("http://localhost/api/connector/readiness")
            .bearer_auth("secret")
            .json(&serde_json::json!({}))
            .send(&service)
            .await;
        assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
        let body: serde_json::Value = resp.take_json().await.unwrap();
        assert_eq!(body["ready"], false);
        assert_eq!(body["findings"][1]["code"], "project_registration_invalid");
        assert_eq!(body["next_action"], "webcodex doctor");
    }
}
