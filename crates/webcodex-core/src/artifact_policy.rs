pub const DOCX_MIME: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
pub const PPTX_MIME: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";
pub const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

pub const SAFE_OCTET_STREAM_ARTIFACT_EXTENSIONS: &[&str] = &[
    ".artifact",
    ".dat",
    ".txt",
    ".csv",
    ".json",
    ".png",
    ".jpg",
    ".jpeg",
    ".webp",
    ".mp3",
    ".mp4",
    ".pdf",
    ".zip",
    ".docx",
    ".pptx",
    ".xlsx",
];

pub fn ooxml_extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        DOCX_MIME => Some(".docx"),
        PPTX_MIME => Some(".pptx"),
        XLSX_MIME => Some(".xlsx"),
        _ => None,
    }
}

/// Maximum decoded image size returned as one native MCP image content block.
///
/// The runner result is JSON/base64 encoded and polling submissions are still
/// bounded by the server's existing 2 MiB text request limit. One decoded MiB
/// leaves enough room for base64 expansion and the small artifact metadata
/// envelope without broadening that global request limit.
pub const MAX_MCP_IMAGE_BYTES: usize = 1024 * 1024;

/// Maximum runner stdout retained for an MCP image artifact response.
///
/// Normal runner output remains capped at 256 KiB. This narrowly larger cap is
/// only selected for `file_read_project_artifact` requests carrying the
/// server-generated `mcp_image` marker.
pub const MAX_MCP_IMAGE_RESPONSE_BYTES: usize = 1536 * 1024;

pub fn has_safe_octet_stream_artifact_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    SAFE_OCTET_STREAM_ARTIFACT_EXTENSIONS
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

pub fn safe_octet_stream_artifact_extensions_csv() -> String {
    SAFE_OCTET_STREAM_ARTIFACT_EXTENSIONS.join(", ")
}

pub fn octet_stream_safe_extension_error() -> String {
    format!(
        "application/octet-stream is only allowed for safe artifact extensions: {}. \
         For smoke tests, use artifacts/smoke/<name>.artifact or \
         artifacts/smoke/<name>.txt, or omit mime_type when appropriate.",
        safe_octet_stream_artifact_extensions_csv()
    )
}
