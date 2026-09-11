use serde_json::{json, Value};
use std::path::PathBuf;
use webcodex_admin::ServerHttpOptions;

use super::{call_runtime_tool_status, resolve_user_api_token};

const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8080";
const PLUGIN_TOOL_NAME: &str = "plugin_tool";
const MAX_HUMAN_TEXT_CHARS: usize = 4096;
const MAX_HUMAN_SCHEMA_CHARS: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginCommonOptions {
    pub(crate) server_url: String,
    pub(crate) server_http: ServerHttpOptions,
    pub(crate) env_file: Option<PathBuf>,
    pub(crate) token_file: Option<PathBuf>,
    pub(crate) token: Option<String>,
    pub(crate) json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginListOptions {
    pub(crate) common: PluginCommonOptions,
    pub(crate) runner: Option<String>,
    pub(crate) plugin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginDescribeOptions {
    pub(crate) common: PluginCommonOptions,
    pub(crate) runner: String,
    pub(crate) plugin: String,
    pub(crate) tool: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginCheckOptions {
    pub(crate) common: PluginCommonOptions,
    pub(crate) runner: String,
    pub(crate) plugin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginReloadOptions {
    pub(crate) common: PluginCommonOptions,
    pub(crate) runner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginCommand {
    List(PluginListOptions),
    Describe(PluginDescribeOptions),
    Check(PluginCheckOptions),
    Reload(PluginReloadOptions),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginCommandOutput {
    pub(crate) stdout: String,
    pub(crate) exit_code: i32,
}

impl PluginCommand {
    fn action(&self) -> &'static str {
        match self {
            Self::List(_) => "list",
            Self::Describe(_) => "describe",
            Self::Check(_) => "check",
            Self::Reload(_) => "reload",
        }
    }

    fn common(&self) -> &PluginCommonOptions {
        match self {
            Self::List(opts) => &opts.common,
            Self::Describe(opts) => &opts.common,
            Self::Check(opts) => &opts.common,
            Self::Reload(opts) => &opts.common,
        }
    }

    fn params(&self) -> Value {
        match self {
            Self::List(opts) => {
                let mut params = json!({"action": "list"});
                if let Some(runner) = opts.runner.as_ref() {
                    params["runner"] = Value::String(runner.clone());
                }
                if let Some(plugin) = opts.plugin.as_ref() {
                    params["plugin"] = Value::String(plugin.clone());
                }
                params
            }
            Self::Describe(opts) => json!({
                "action": "describe",
                "runner": opts.runner,
                "plugin": opts.plugin,
                "tool": opts.tool,
            }),
            Self::Check(opts) => json!({
                "action": "check",
                "runner": opts.runner,
                "plugin": opts.plugin,
            }),
            Self::Reload(opts) => json!({
                "action": "reload",
                "runner": opts.runner,
            }),
        }
    }

    fn is_consequential_management(&self) -> bool {
        matches!(self, Self::Check(_) | Self::Reload(_))
    }
}

#[derive(Debug, Default)]
struct ParsedFlags {
    server_url: Option<String>,
    proxy: Option<String>,
    no_system_proxy: bool,
    no_system_proxy_seen: bool,
    env_file: Option<PathBuf>,
    token_file: Option<PathBuf>,
    token: Option<String>,
    json: bool,
    json_seen: bool,
    runner: Option<String>,
    plugin: Option<String>,
    tool: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct AllowedIdentityFlags {
    runner: bool,
    plugin: bool,
    tool: bool,
}

pub(crate) fn parse_plugin_command(
    command: &str,
    args: &[String],
) -> Result<PluginCommand, String> {
    let allowed = match command {
        "list" => AllowedIdentityFlags {
            runner: true,
            plugin: true,
            tool: false,
        },
        "describe" => AllowedIdentityFlags {
            runner: true,
            plugin: true,
            tool: true,
        },
        "check" => AllowedIdentityFlags {
            runner: true,
            plugin: true,
            tool: false,
        },
        "reload" => AllowedIdentityFlags {
            runner: true,
            plugin: false,
            tool: false,
        },
        other => return Err(format!("unknown plugin subcommand: {other}")),
    };
    let flags = parse_flags(command, args, allowed)?;
    let common = flags.common()?;
    match command {
        "list" => {
            if flags.plugin.is_some() && flags.runner.is_none() {
                return Err("plugin list requires --runner when --plugin is provided".to_string());
            }
            Ok(PluginCommand::List(PluginListOptions {
                common,
                runner: flags.runner,
                plugin: flags.plugin,
            }))
        }
        "describe" => Ok(PluginCommand::Describe(PluginDescribeOptions {
            common,
            runner: required_identity(flags.runner, "--runner", command)?,
            plugin: required_identity(flags.plugin, "--plugin", command)?,
            tool: required_identity(flags.tool, "--tool", command)?,
        })),
        "check" => Ok(PluginCommand::Check(PluginCheckOptions {
            common,
            runner: required_identity(flags.runner, "--runner", command)?,
            plugin: required_identity(flags.plugin, "--plugin", command)?,
        })),
        "reload" => Ok(PluginCommand::Reload(PluginReloadOptions {
            common,
            runner: required_identity(flags.runner, "--runner", command)?,
        })),
        _ => unreachable!("closed plugin command vocabulary"),
    }
}

fn parse_flags(
    command: &str,
    args: &[String],
    allowed: AllowedIdentityFlags,
) -> Result<ParsedFlags, String> {
    let mut flags = ParsedFlags::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--server-url" => {
                let value = flag_value(args, &mut index, arg)?;
                set_once(&mut flags.server_url, value, arg)?;
            }
            "--proxy" => {
                let value = flag_value(args, &mut index, arg)?;
                set_once(&mut flags.proxy, value, arg)?;
            }
            "--no-system-proxy" => {
                if flags.no_system_proxy_seen {
                    return Err("--no-system-proxy may be specified only once".to_string());
                }
                flags.no_system_proxy_seen = true;
                flags.no_system_proxy = true;
            }
            "--env-file" => {
                let value = flag_value(args, &mut index, arg)?;
                set_path_once(&mut flags.env_file, value, arg)?;
            }
            "--token-file" => {
                let value = flag_value(args, &mut index, arg)?;
                set_path_once(&mut flags.token_file, value, arg)?;
            }
            "--token" => {
                let value = flag_value(args, &mut index, arg)?;
                set_once(&mut flags.token, value, arg)?;
            }
            "--json" => {
                if flags.json_seen {
                    return Err("--json may be specified only once".to_string());
                }
                flags.json_seen = true;
                flags.json = true;
            }
            "--runner" if allowed.runner => {
                let value = flag_value(args, &mut index, arg)?;
                set_once(&mut flags.runner, value, arg)?;
            }
            "--plugin" if allowed.plugin => {
                let value = flag_value(args, &mut index, arg)?;
                set_once(&mut flags.plugin, value, arg)?;
            }
            "--tool" if allowed.tool => {
                let value = flag_value(args, &mut index, arg)?;
                set_once(&mut flags.tool, value, arg)?;
            }
            other => return Err(format!("unknown plugin {command} flag: {other}")),
        }
        index += 1;
    }
    Ok(flags)
}

impl ParsedFlags {
    fn common(&self) -> Result<PluginCommonOptions, String> {
        let server_http = ServerHttpOptions {
            proxy: self.proxy.clone(),
            no_system_proxy: self.no_system_proxy,
        };
        server_http.validate()?;
        let server_url = self
            .server_url
            .clone()
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
        if server_url.trim().is_empty() {
            return Err("--server-url cannot be empty".to_string());
        }
        Ok(PluginCommonOptions {
            server_url,
            server_http,
            env_file: self.env_file.clone(),
            token_file: self.token_file.clone(),
            token: self.token.clone(),
            json: self.json,
        })
    }
}

fn flag_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    let value = args
        .get(*index)
        .ok_or_else(|| format!("{flag} requires a value"))?;
    if value.is_empty() {
        return Err(format!("{flag} cannot be empty"));
    }
    Ok(value.clone())
}

fn set_once(target: &mut Option<String>, value: String, flag: &str) -> Result<(), String> {
    if target.is_some() {
        return Err(format!("{flag} may be specified only once"));
    }
    *target = Some(value);
    Ok(())
}

fn set_path_once(target: &mut Option<PathBuf>, value: String, flag: &str) -> Result<(), String> {
    if target.is_some() {
        return Err(format!("{flag} may be specified only once"));
    }
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() {
        return Err(format!("{flag} cannot be empty"));
    }
    *target = Some(path);
    Ok(())
}

fn required_identity(value: Option<String>, flag: &str, command: &str) -> Result<String, String> {
    let value = value.ok_or_else(|| format!("plugin {command} requires {flag}"))?;
    if value.trim().is_empty() {
        return Err(format!("{flag} cannot be empty"));
    }
    Ok(value)
}

pub(crate) async fn run_plugin_command(
    command: PluginCommand,
) -> Result<PluginCommandOutput, String> {
    let common = command.common();
    let token = resolve_user_api_token(&common.token, &common.token_file, &common.env_file)?;
    let action = command.action();
    let response = call_runtime_tool_status(
        &common.server_url,
        &common.server_http,
        token.as_deref(),
        PLUGIN_TOOL_NAME,
        command.params(),
    )
    .await;

    let (status, content_type, value) = match response {
        Ok(response) => response,
        Err(error) => return Err(transport_error(&command, &error)),
    };

    if status == 401 {
        return Err("plugin request was rejected with HTTP 401; provide a user/API bearer credential accepted by this WebCodex Server".to_string());
    }
    if status == 403 {
        let scope = if matches!(command, PluginCommand::List(_) | PluginCommand::Describe(_)) {
            "plugin:inspect"
        } else {
            "plugin:manage"
        };
        let extra = if scope == "plugin:manage" {
            "; --oauth-local-plugins grants inspect/invoke only and does not grant plugin:manage"
        } else {
            ""
        };
        return Err(format!(
            "plugin request was rejected with HTTP 403; use a credential explicitly granted {scope}{extra}"
        ));
    }

    let Some(value) = value else {
        return Err(response_shape_error(
            &command,
            format!("HTTP {status} returned non-JSON content ({content_type})"),
        ));
    };

    let Some(success) = value.get("success").and_then(Value::as_bool) else {
        return Err(response_shape_error(
            &command,
            format!("HTTP {status} returned a malformed runtime tool response"),
        ));
    };
    let output = value.get("output").cloned();

    if !success {
        if let Some(output) = output {
            let stdout = render_tool_failure(action, &output, common.json)?;
            return Ok(PluginCommandOutput {
                stdout,
                exit_code: 1,
            });
        }
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .map(|text| bounded_line(text, MAX_HUMAN_TEXT_CHARS))
            .unwrap_or_else(|| format!("HTTP {status} runtime tool failure"));
        return Err(response_shape_error(&command, error));
    }

    if !(200..300).contains(&status) {
        return Err(response_shape_error(
            &command,
            format!("runtime tool reported success with unexpected HTTP {status}"),
        ));
    }
    let output = output.ok_or_else(|| {
        response_shape_error(
            &command,
            "runtime tool response is missing output".to_string(),
        )
    })?;

    let exit_code = command_exit_code(&command, &output)?;
    let stdout = if common.json {
        render_json(&output)?
    } else {
        render_human(&command, &output)?
    };
    Ok(PluginCommandOutput { stdout, exit_code })
}

fn command_exit_code(command: &PluginCommand, output: &Value) -> Result<i32, String> {
    match command {
        PluginCommand::Check(_) => output
            .get("ready")
            .and_then(Value::as_bool)
            .map(|ready| if ready { 0 } else { 2 })
            .ok_or_else(|| "malformed plugin check output: missing boolean ready".to_string()),
        PluginCommand::Reload(_) => output
            .get("failures")
            .and_then(Value::as_array)
            .map(|failures| if failures.is_empty() { 0 } else { 2 })
            .ok_or_else(|| "malformed plugin reload output: missing failures array".to_string()),
        PluginCommand::List(_) | PluginCommand::Describe(_) => Ok(0),
    }
}

fn transport_error(command: &PluginCommand, error: &str) -> String {
    let error = bounded_line(error, MAX_HUMAN_TEXT_CHARS);
    if command.is_consequential_management() {
        format!(
            "plugin {} request failed after the Server request began; outcome may be unknown. Do not retry automatically; observe current Plugin state before retrying. {}",
            command.action(),
            error
        )
    } else {
        format!("plugin {} request failed: {}", command.action(), error)
    }
}

fn response_shape_error(command: &PluginCommand, detail: String) -> String {
    let detail = bounded_line(&detail, MAX_HUMAN_TEXT_CHARS);
    if command.is_consequential_management() {
        format!(
            "plugin {} response could not be confirmed; outcome may be unknown. Do not retry automatically; observe current Plugin state before retrying. {}",
            command.action(),
            detail
        )
    } else {
        format!(
            "plugin {} response could not be read: {}",
            command.action(),
            detail
        )
    }
}

fn render_tool_failure(action: &str, output: &Value, json_output: bool) -> Result<String, String> {
    if json_output {
        return render_json(output);
    }
    let mut lines = vec![format!("Plugin {action}: failed")];
    if let Some(code) = output
        .pointer("/error/code")
        .and_then(Value::as_str)
        .or_else(|| output.get("failure_kind").and_then(Value::as_str))
    {
        lines.push(format!(
            "Code: {}",
            bounded_line(code, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    if let Some(message) = output.pointer("/error/message").and_then(Value::as_str) {
        lines.push(format!(
            "Message: {}",
            bounded_line(message, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    if let Some(certainty) = output.get("dispatch_certainty").and_then(Value::as_str) {
        lines.push(format!(
            "Dispatch certainty: {}",
            bounded_line(certainty, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    if let Some(recovery) = output.get("recovery").and_then(Value::as_str) {
        lines.push(format!(
            "Recovery: {}",
            bounded_line(recovery, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn render_human(command: &PluginCommand, output: &Value) -> Result<String, String> {
    match command {
        PluginCommand::List(opts) => render_list(opts, output),
        PluginCommand::Describe(_) => render_describe(output),
        PluginCommand::Check(_) => render_check(output),
        PluginCommand::Reload(_) => render_reload(output),
    }
}

fn render_list(opts: &PluginListOptions, output: &Value) -> Result<String, String> {
    if opts.runner.is_none() {
        let runners = array_field(output, "runners", "plugin list")?;
        let mut lines = vec![format!("Runners: {}", runners.len())];
        for runner in runners {
            let id = string_field(runner, "runner", "plugin list runner")?;
            let name = runner.get("name").and_then(Value::as_str);
            lines.push(match name {
                Some(name) => format!(
                    "  {}  {}",
                    bounded_line(id, MAX_HUMAN_TEXT_CHARS),
                    bounded_line(name, MAX_HUMAN_TEXT_CHARS)
                ),
                None => format!("  {}", bounded_line(id, MAX_HUMAN_TEXT_CHARS)),
            });
        }
        return Ok(format!("{}\n", lines.join("\n")));
    }

    if opts.plugin.is_none() {
        let runner = string_field(output, "runner", "plugin list")?;
        let plugins = array_field(output, "plugins", "plugin list")?;
        let mut lines = vec![
            format!("Runner: {}", bounded_line(runner, MAX_HUMAN_TEXT_CHARS)),
            format!("Plugins: {}", plugins.len()),
        ];
        for plugin in plugins {
            lines.push(render_provider_line(plugin)?);
        }
        return Ok(format!("{}\n", lines.join("\n")));
    }

    let runner = string_field(output, "runner", "plugin list")?;
    let plugin = string_field(output, "plugin", "plugin list")?;
    let status = string_field(output, "status", "plugin list")?;
    let tools = array_field(output, "tools", "plugin list")?;
    let mut lines = vec![
        format!("Plugin: {}", bounded_line(plugin, MAX_HUMAN_TEXT_CHARS)),
        format!("Runner: {}", bounded_line(runner, MAX_HUMAN_TEXT_CHARS)),
        format!("Status: {}", bounded_line(status, MAX_HUMAN_TEXT_CHARS)),
        format!("Tools: {}", tools.len()),
    ];
    for tool in tools {
        lines.push(render_tool_line(tool)?);
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn render_describe(output: &Value) -> Result<String, String> {
    let runner = string_field(output, "runner", "plugin describe")?;
    let plugin = string_field(output, "plugin", "plugin describe")?;
    let tool = output
        .get("tool")
        .and_then(Value::as_object)
        .ok_or_else(|| "malformed plugin describe output: missing tool object".to_string())?;
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "malformed plugin describe output: missing tool.name".to_string())?;
    let binding = string_field(output, "binding", "plugin describe")?;
    let mut lines = vec![
        format!("Plugin: {}", bounded_line(plugin, MAX_HUMAN_TEXT_CHARS)),
        format!("Runner: {}", bounded_line(runner, MAX_HUMAN_TEXT_CHARS)),
        format!("Tool: {}", bounded_line(name, MAX_HUMAN_TEXT_CHARS)),
    ];
    if let Some(title) = tool.get("title").and_then(Value::as_str) {
        lines.push(format!(
            "Title: {}",
            bounded_line(title, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    if let Some(description) = tool.get("description").and_then(Value::as_str) {
        lines.push(format!(
            "Description: {}",
            bounded_line(description, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    if let Some(annotations) = tool.get("annotations") {
        lines.push(format!(
            "Annotations: {}",
            bounded_json(annotations, MAX_HUMAN_SCHEMA_CHARS)?
        ));
    }
    let input = tool
        .get("inputSchema")
        .ok_or_else(|| "malformed plugin describe output: missing tool.inputSchema".to_string())?;
    lines.push(format!(
        "Input schema: {}",
        bounded_json(input, MAX_HUMAN_SCHEMA_CHARS)?
    ));
    if let Some(output_schema) = tool.get("outputSchema") {
        lines.push(format!(
            "Output schema: {}",
            bounded_json(output_schema, MAX_HUMAN_SCHEMA_CHARS)?
        ));
    }
    lines.push(format!(
        "Binding: {}",
        bounded_line(binding, MAX_HUMAN_TEXT_CHARS)
    ));
    Ok(format!("{}\n", lines.join("\n")))
}

fn render_check(output: &Value) -> Result<String, String> {
    let runner = string_field(output, "runner", "plugin check")?;
    let plugin = string_field(output, "plugin", "plugin check")?;
    let ready = output
        .get("ready")
        .and_then(Value::as_bool)
        .ok_or_else(|| "malformed plugin check output: missing boolean ready".to_string())?;
    let tools = array_field(output, "tools", "plugin check")?;
    let mut lines = vec![
        format!("Plugin: {}", bounded_line(plugin, MAX_HUMAN_TEXT_CHARS)),
        format!("Runner: {}", bounded_line(runner, MAX_HUMAN_TEXT_CHARS)),
        format!("Status: {}", if ready { "ready" } else { "not ready" }),
    ];
    if let Some(phase) = output.get("phase").and_then(Value::as_str) {
        lines.push(format!(
            "Phase: {}",
            bounded_line(phase, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    if let Some(code) = output.get("code").and_then(Value::as_str) {
        lines.push(format!(
            "Code: {}",
            bounded_line(code, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    if let Some(detail) = output.get("detail").and_then(Value::as_str) {
        lines.push(format!(
            "Detail: {}",
            bounded_line(detail, MAX_HUMAN_TEXT_CHARS)
        ));
    }
    if let Some(diagnostic) = output.get("diagnostic") {
        lines.push(format!(
            "Diagnostic: {}",
            bounded_json(diagnostic, MAX_HUMAN_TEXT_CHARS)?
        ));
    }
    lines.push(format!("Tools: {}", tools.len()));
    for tool in tools {
        lines.push(render_tool_line(tool)?);
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn render_reload(output: &Value) -> Result<String, String> {
    let runner = string_field(output, "runner", "plugin reload")?;
    let plugins = array_field(output, "plugins", "plugin reload")?;
    let failures = array_field(output, "failures", "plugin reload")?;
    let mut lines = vec![
        format!("Runner: {}", bounded_line(runner, MAX_HUMAN_TEXT_CHARS)),
        format!(
            "Status: {}",
            if failures.is_empty() {
                "ready"
            } else {
                "rejected"
            }
        ),
        format!("Plugins: {}", plugins.len()),
    ];
    for plugin in plugins {
        lines.push(render_provider_line(plugin)?);
    }
    if !failures.is_empty() {
        lines.push(format!("Failures: {}", failures.len()));
        for failure in failures {
            let provider = failure
                .get("provider_id")
                .and_then(Value::as_str)
                .unwrap_or("[unknown]");
            let code = failure
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            lines.push(format!(
                "  {}  {}",
                bounded_line(provider, MAX_HUMAN_TEXT_CHARS),
                bounded_line(code, MAX_HUMAN_TEXT_CHARS)
            ));
        }
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn render_provider_line(provider: &Value) -> Result<String, String> {
    let id = string_field(provider, "plugin", "Plugin provider")?;
    let status = string_field(provider, "status", "Plugin provider")?;
    let name = provider.get("name").and_then(Value::as_str).unwrap_or("");
    let error_code = provider.get("errorCode").and_then(Value::as_str);
    let mut line = format!(
        "  {}  {}",
        bounded_line(id, MAX_HUMAN_TEXT_CHARS),
        bounded_line(status, MAX_HUMAN_TEXT_CHARS)
    );
    if !name.is_empty() {
        line.push_str("  ");
        line.push_str(&bounded_line(name, MAX_HUMAN_TEXT_CHARS));
    }
    if let Some(code) = error_code.filter(|code| !code.is_empty()) {
        line.push_str("  code=");
        line.push_str(&bounded_line(code, MAX_HUMAN_TEXT_CHARS));
    }
    Ok(line)
}

fn render_tool_line(tool: &Value) -> Result<String, String> {
    let name = string_field(tool, "name", "Plugin tool")?;
    let title = tool.get("title").and_then(Value::as_str).unwrap_or("");
    Ok(if title.is_empty() {
        format!("  {}", bounded_line(name, MAX_HUMAN_TEXT_CHARS))
    } else {
        format!(
            "  {}  {}",
            bounded_line(name, MAX_HUMAN_TEXT_CHARS),
            bounded_line(title, MAX_HUMAN_TEXT_CHARS)
        )
    })
}

fn array_field<'a>(value: &'a Value, field: &str, context: &str) -> Result<&'a Vec<Value>, String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("malformed {context} output: missing {field} array"))
}

fn string_field<'a>(value: &'a Value, field: &str, context: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("malformed {context} output: missing {field}"))
}

fn render_json(value: &Value) -> Result<String, String> {
    serde_json::to_string_pretty(value)
        .map(|text| format!("{text}\n"))
        .map_err(|error| format!("failed to encode Plugin JSON output: {error}"))
}

fn bounded_line(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(max_chars) {
        output.push(if ch.is_control() { ' ' } else { ch });
    }
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

fn bounded_json(value: &Value, max_chars: usize) -> Result<String, String> {
    let encoded = serde_json::to_string(value)
        .map_err(|error| format!("failed to encode Plugin metadata: {error}"))?;
    Ok(bounded_line(&encoded, max_chars))
}
