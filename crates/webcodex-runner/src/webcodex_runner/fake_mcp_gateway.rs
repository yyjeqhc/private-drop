// Standalone newline-delimited stdio MCP fixture. Tests compile this file
// directly with rustc so no closed-source or installed MCP server is needed.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

fn main() -> io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let scenario = args.first().map(String::as_str).unwrap_or("normal");
    let marker = args.get(1).map(Path::new);
    append(marker, "start\n")?;
    if scenario == "execution_context" {
        append(
            marker,
            if env::var("GITHUB_TOKEN").as_deref() == Ok("github-provider-secret-value") {
                "github-env-ok\n"
            } else {
                "github-env-bad\n"
            },
        )?;
        append(
            marker,
            if env::var("WEBCODEX_MCP_MAPPED_CHILD").as_deref()
                == Ok("mapped-provider-secret-value")
            {
                "mapped-env-ok\n"
            } else {
                "mapped-env-bad\n"
            },
        )?;
        append(
            marker,
            if env::var_os("WEBCODEX_MCP_UNLISTED").is_none() {
                "unlisted-env-cleared\n"
            } else {
                "unlisted-env-leaked\n"
            },
        )?;
        append(
            marker,
            if env::var_os("PATH").is_none() {
                "path-cleared\n"
            } else {
                "path-leaked\n"
            },
        )?;
        let cwd_matches = match (args.get(2), env::current_dir()) {
            (Some(expected), Ok(current)) => {
                std::fs::canonicalize(current).ok() == std::fs::canonicalize(expected).ok()
            }
            _ => false,
        };
        append(marker, if cwd_matches { "cwd-ok\n" } else { "cwd-bad\n" })?;
    }
    let mut reader = BufReader::new(io::stdin().lock());
    let mut writer = io::stdout().lock();
    let mut calls = 0usize;
    let mut lists = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let method = string_field(&line, "method").unwrap_or_default();
        let id = u64_field(&line, "id").unwrap_or(0);
        match method.as_str() {
            "initialize" => {
                append(marker, "initialize\n")?;
                if scenario == "init_crash" {
                    return Ok(());
                }
                if scenario == "init_timeout" {
                    thread::sleep(Duration::from_secs(3));
                }
                let capabilities = if scenario == "init_missing_tools" {
                    r#"{}"#
                } else {
                    r#"{"tools":{}}"#
                };
                send(
                    &mut writer,
                    &format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-06-18","capabilities":{capabilities},"serverInfo":{{"name":"fake-bridge","version":"1"}}}}}}"#
                    ),
                )?;
            }
            "notifications/initialized" => append(marker, "initialized\n")?,
            "tools/list" => {
                lists += 1;
                append(marker, "list\n")?;
                if scenario == "notifications" {
                    send(
                        &mut writer,
                        r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#,
                    )?;
                }
                if scenario == "notification_flood" {
                    for progress in 0..40 {
                        send(
                            &mut writer,
                            &format!(
                                r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{{"progressToken":"flood","progress":{progress}}}}}"#
                            ),
                        )?;
                    }
                    continue;
                }
                if scenario == "oversized_message" {
                    send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"padding":"{}","tools":[]}}}}"#,
                            "x".repeat(1024 * 1024)
                        ),
                    )?;
                    continue;
                }
                if scenario == "malformed" {
                    send(&mut writer, "{not-json")?;
                    continue;
                }
                if scenario == "unknown_id" {
                    send(
                        &mut writer,
                        r#"{"jsonrpc":"2.0","id":999999,"result":{"tools":[]}}"#,
                    )?;
                    continue;
                }
                if scenario == "paginated_list" {
                    send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[],"nextCursor":"more"}}}}"#
                        ),
                    )?;
                    continue;
                }
                let description = if scenario == "bad_tools" {
                    "x".repeat(5 * 1024)
                } else {
                    "Persistent fake echo".to_string()
                };
                let value_type = if scenario == "schema_change" && lists >= 2 {
                    "number"
                } else {
                    "string"
                };
                send(
                    &mut writer,
                    &format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","description":"{description}","inputSchema":{{"type":"object","properties":{{"value":{{"type":"{value_type}"}}}}}}}}]}}}}"#
                    ),
                )?;
                if scenario == "notifications" {
                    send(
                        &mut writer,
                        r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"queued"}}"#,
                    )?;
                }
                if scenario == "duplicate_id" {
                    send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[]}}}}"#
                        ),
                    )?;
                }
            }
            "tools/call" => {
                calls += 1;
                append(marker, "call\n")?;
                if scenario == "meta_forbidden"
                    && [
                        "\"_meta\"",
                        "progressToken",
                        "clientInfo",
                        "protocolVersion",
                        "clientCapabilities",
                    ]
                    .iter()
                    .any(|field| line.contains(field))
                {
                    send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32602,"message":"caller metadata crossed gateway boundary"}}}}"#
                        ),
                    )?;
                    continue;
                }
                match scenario {
                    "crash" => return Ok(()),
                    "notifications" => {
                        send(
                            &mut writer,
                            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"call","progress":1}}"#,
                        )?;
                        send(
                            &mut writer,
                            &format!(
                                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"call-{calls}"}}],"structuredContent":{{"call":{calls}}},"isError":false}}}}"#
                            ),
                        )?;
                    }
                    "callback" => send(
                        &mut writer,
                        r#"{"jsonrpc":"2.0","id":9000,"method":"sampling/createMessage","params":{}}"#,
                    )?,
                    "timeout" => {
                        thread::sleep(Duration::from_secs(3));
                    }
                    "slow" => {
                        thread::sleep(Duration::from_millis(1_250));
                        send(
                            &mut writer,
                            &format!(
                                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"slow"}}],"isError":false}}}}"#
                            ),
                        )?;
                    }
                    "rpc_error" => send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32001,"message":"fixture error"}}}}"#
                        ),
                    )?,
                    "large_result" => send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"{}"}}],"structuredContent":{{"payload":"{}"}},"isError":false}}}}"#,
                            "x".repeat(192 * 1024),
                            "y".repeat(192 * 1024)
                        ),
                    )?,
                    "bad_result" => send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"image","data":"AA==","mimeType":"image/png"}}]}}}}"#
                        ),
                    )?,
                    "oversized_result" => send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
                            "x".repeat(520 * 1024)
                        ),
                    )?,
                    _ => send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"call-{calls}"}}],"structuredContent":{{"call":{calls}}},"isError":false}}}}"#
                        ),
                    )?,
                }
            }
            _ => {}
        }
    }
}

fn send(writer: &mut impl Write, message: &str) -> io::Result<()> {
    writer.write_all(message.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn append(path: Option<&Path>, value: &str) -> io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(value.as_bytes())
}

fn string_field(body: &str, field: &str) -> Option<String> {
    let prefix = format!(r#""{field}":"#);
    let start = body.find(&prefix)? + prefix.len();
    let value = body.get(start..)?.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

fn u64_field(body: &str, field: &str) -> Option<u64> {
    let prefix = format!(r#""{field}":"#);
    let start = body.find(&prefix)? + prefix.len();
    let digits = body
        .get(start..)?
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}
