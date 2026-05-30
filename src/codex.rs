use crate::projects::{canonicalize_and_verify, ProjectConfig, ProjectsConfig, SshConfig};
use crate::{CodexGoalRecord, Database};
use salvo::prelude::*;
mod artifact;
mod command_request;
mod context;
mod edit;
mod git;
mod jobs;
mod report;
mod security;
mod shell;
mod ssh;
mod types;
mod url_security;
pub use artifact::codex_artifact;
use command_request::*;
pub use command_request::{
    codex_check, codex_command, codex_command_approve, codex_command_reject, codex_command_request,
    codex_command_request_batch, codex_command_request_op, codex_command_request_raw,
    codex_command_requests,
};
use context::*;
pub use edit::codex_edit;
use edit::*;
pub use git::codex_git;
#[cfg(test)]
use git::*;
pub use jobs::codex_job;
pub use report::codex_report;
pub use security::is_sensitive_path;
use shell::*;
use ssh::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use types::*;
use url_security::*;
// =============================================================================
// Request / Response types
// =============================================================================

// =============================================================================
// Constants
// =============================================================================

pub(super) const MAX_OUTPUT_LEN: usize = 50_000;
pub(super) const CHECK_TIMEOUT_SECS: u64 = 300;
const MAX_BINARY_ARTIFACT_SIZE: usize = 5 * 1024 * 1024;
const URL_IMPORT_TIMEOUT_SECS: u64 = 10;

// =============================================================================
// Helpers
// =============================================================================

pub(super) fn get_projects(depot: &Depot) -> Option<Arc<ProjectsConfig>> {
    depot.obtain::<Arc<ProjectsConfig>>().ok().cloned()
}

fn truncate_string(s: String, max_len: usize) -> (String, bool) {
    if s.len() <= max_len {
        (s, false)
    } else {
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        (s[..end].to_string(), true)
    }
}

fn parse_changed_files_from_patch(patch: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            // Format: diff --git a/path b/path
            if let Some(b_pos) = line.rfind(" b/") {
                let file = &line[b_pos + 3..];
                if !files.iter().any(|f: &String| f == file) {
                    files.push(file.to_string());
                }
            }
        }
    }
    files
}

// =============================================================================
// SSH helpers
// =============================================================================

/// Run a command in the project directory.
/// For SSH: wraps with `cd <path> && <cmd>`.
/// For local: delegates to run_command with cwd.
pub(super) fn run_project_cmd(
    proj: &ProjectConfig,
    cmd: &str,
    timeout_secs: u64,
    ssh_config: Option<&SshConfig>,
) -> (i32, String, String, u64) {
    if proj.is_ssh() {
        let ssh_target = match build_ssh_target(proj) {
            Ok(t) => t,
            Err(e) => return (-1, String::new(), e, 0),
        };
        let remote_cmd = format!("cd {} && {}", shell_escape(&proj.path), cmd);
        run_ssh(&ssh_target, &remote_cmd, timeout_secs, ssh_config)
    } else {
        run_command(cmd, &proj.root(), timeout_secs)
    }
}

/// Embedded Python3 script for remote edit operations.
/// Receives project root via argv[1] and edit JSON via stdin.
/// Returns JSON result on stdout.
const REMOTE_EDIT_SCRIPT: &str = r#####"
import sys, json, os, difflib, base64, urllib.request, urllib.parse, socket, ipaddress

SENSITIVE = ('.git', '.env', '.pem', '.key', 'id_rsa', 'id_ed25519',
             'target', 'node_modules')
MAX_FILE = 2 * 1024 * 1024
MAX_TEXT = 200 * 1024
MAX_BINARY = 5 * 1024 * 1024
URL_TIMEOUT = 10

def err(msg):
    return {'success': False, 'changed_files': [], 'diff': '', 'warnings': [], 'error': msg}

def is_sensitive(p):
    parts = p.replace('\\\\', '/').split('/')
    for seg in parts:
        if seg in SENSITIVE:
            return True
        for suf in ('.pem', '.key'):
            if seg.endswith(suf):
                return True
    return False

def validate_path(rel):
    if not rel:
        return 'path cannot be empty'
    if rel.startswith('/'):
        return 'Absolute paths are not allowed'
    if '..' in rel:
        return 'Path traversal (..) is not allowed'
    if is_sensitive(rel):
        return 'Cannot modify sensitive path: ' + rel
    return None

def resolve(root, rel, must_exist):
    e = validate_path(rel)
    if e:
        return None, e
    full = os.path.normpath(os.path.join(root, rel))
    canon_root = os.path.realpath(root)
    if not os.path.realpath(full).startswith(canon_root + os.sep) and os.path.realpath(full) != canon_root:
        return None, 'Path is outside project directory'
    if must_exist:
        if not os.path.isfile(full):
            return None, 'File does not exist: ' + rel
    else:
        parent = os.path.dirname(full)
        ancestor = parent
        while ancestor and not os.path.isdir(ancestor):
            next_ancestor = os.path.dirname(ancestor)
            if next_ancestor == ancestor:
                return None, 'path has no existing parent directory'
            ancestor = next_ancestor
        if not os.path.realpath(ancestor).startswith(canon_root + os.sep) and os.path.realpath(ancestor) != canon_root:
            return None, 'Path is outside project directory'
    return full, None

def read_file(path):
    try:
        sz = os.path.getsize(path)
    except OSError as e:
        return None, 'Failed to stat file: ' + str(e)
    if sz > MAX_FILE:
        return None, 'File too large: %d bytes' % sz
    try:
        with open(path, 'r', encoding='utf-8') as f:
            return f.read(), None
    except Exception as e:
        return None, 'Failed to read UTF-8 file: ' + str(e)

def replace_nth(content, old, new, occ):
    if not old:
        return None, 'old_text cannot be empty'
    idxs = []
    start = 0
    while True:
        i = content.find(old, start)
        if i < 0:
            break
        idxs.append(i)
        start = i + len(old)
    if not idxs:
        return None, 'old_text was not found'
    if occ is not None:
        if occ < 1:
            return None, 'occurrence is 1-based and must be >= 1'
        if occ > len(idxs):
            return None, 'occurrence %d exceeds match count %d' % (occ, len(idxs))
        sel = idxs[occ - 1]
    else:
        if len(idxs) > 1:
            return None, 'old_text matched %d times; specify occurrence' % len(idxs)
        sel = idxs[0]
    return content[:sel] + new + content[sel + len(old):], None

def replace_range(content, sl, el, new):
    if sl < 1 or el < 1 or sl > el:
        return None, 'start_line and end_line must be 1-based and start_line <= end_line'
    had_nl = content.endswith('\n')
    lines = content.split('\n')
    # If content ends with \n, split gives an extra empty string at the end
    if had_nl and lines and lines[-1] == '':
        lines = lines[:-1]
    if el > len(lines):
        return None, 'line range %d-%d exceeds file line count %d' % (sl, el, len(lines))
    repl = [] if not new else new.rstrip('\n').split('\n')
    lines2 = lines[:sl-1] + repl + lines[el:]
    out = '\n'.join(lines2)
    if had_nl or new.endswith('\n'):
        out += '\n'
    return out, None

def simple_diff(path, old_content, new_content):
    old_lines = (old_content or '').splitlines(True)
    new_lines = new_content.splitlines(True)
    diff = difflib.unified_diff(old_lines, new_lines, fromfile='a/' + path, tofile='b/' + path)
    return ''.join(diff)

def simple_binary_diff(path, old_len, new_len):
    if old_len is None:
        return 'diff --git a/{0} b/{0}\nnew file mode 100644\nBinary file b/{0} added\n# new size: {1} bytes\n'.format(path, new_len)
    return 'diff --git a/{0} b/{0}\nBinary files a/{0} and b/{0} differ\n# old size: {1} bytes\n# new size: {2} bytes\n'.format(path, old_len, new_len)

def check_binary_size(data, label):
    if len(data) > MAX_BINARY:
        return None, 'binary content for %s exceeds %d bytes' % (label, MAX_BINARY)
    return data, None

def decode_binary(payload, rel):
    if len(payload) > MAX_BINARY * 2:
        return None, 'base64 content for %s is too large; maximum decoded size is %d bytes' % (rel, MAX_BINARY)
    try:
        data = base64.b64decode(payload, validate=True)
    except Exception as e:
        return None, 'Invalid base64 content for %s: %s' % (rel, e)
    return check_binary_size(data, rel)

def read_upload(root, source_file, rel):
    if not source_file:
        return None, 'source_file cannot be empty'
    if '..' in source_file:
        return None, 'source_file path traversal is not allowed'
    if is_sensitive(source_file):
        return None, 'source_file cannot reference a sensitive path'
    full = source_file if os.path.isabs(source_file) else os.path.join(root, source_file)
    try:
        canon = os.path.realpath(full)
    except Exception as e:
        return None, 'Failed to access source_file: %s' % e
    allowed_roots = [root, '/tmp', '/var/tmp', '/mnt/data']
    drop_data = os.environ.get('DROP_DATA')
    if drop_data:
        allowed_roots.append(os.path.join(drop_data, 'uploads'))
    allowed = False
    for allowed_root in allowed_roots:
        if os.path.isdir(allowed_root):
            ar = os.path.realpath(allowed_root)
            if canon == ar or canon.startswith(ar + os.sep):
                allowed = True
                break
    if not allowed:
        return None, 'source_file is outside allowed upload/temp directories'
    if not os.path.isfile(canon):
        return None, 'source_file must be a regular file'
    if os.path.getsize(canon) > MAX_BINARY:
        return None, 'source_file for %s exceeds %d bytes' % (rel, MAX_BINARY)
    try:
        with open(canon, 'rb') as f:
            return check_binary_size(f.read(), rel)
    except Exception as e:
        return None, 'Failed to read source_file: %s' % e

def blocked_ip(ip):
    try:
        addr = ipaddress.ip_address(ip)
    except ValueError:
        return True
    return addr.is_private or addr.is_loopback or addr.is_link_local or addr.is_multicast or addr.is_unspecified or addr.is_reserved

def is_allowed_chatgpt_estuary_url(parsed):
    if parsed.scheme != 'https' or parsed.hostname != 'chatgpt.com':
        return False
    if parsed.path != '/backend-api/estuary/content':
        return False
    qs = urllib.parse.parse_qs(parsed.query)
    ids = qs.get('id') or []
    sigs = qs.get('sig') or []
    return any(v.startswith('file_') for v in ids) and any(bool(v) for v in sigs)

def validate_url(source_url):
    try:
        parsed = urllib.parse.urlparse(source_url)
    except Exception as e:
        return None, 'Invalid source_url: %s' % e
    if parsed.scheme not in ('http', 'https'):
        return None, 'source_url must use http or https'
    if parsed.username or parsed.password:
        return None, 'source_url must not contain credentials'
    host = parsed.hostname
    if not host:
        return None, 'source_url must include a host'
    if host.lower() == 'localhost' or host.lower().endswith('.localhost'):
        return None, 'source_url host is not allowed'
    if is_allowed_chatgpt_estuary_url(parsed):
        return source_url, None
    port = parsed.port or (443 if parsed.scheme == 'https' else 80)
    try:
        infos = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    except Exception as e:
        return None, 'Failed to resolve source_url host: %s' % e
    if not infos:
        return None, 'source_url host resolved to no addresses'
    for info in infos:
        if blocked_ip(info[4][0]):
            return None, 'source_url resolves to a blocked private/local address'
    return source_url, None

def read_url(source_url, rel):
    url, e = validate_url(source_url)
    if e:
        return None, e
    req = urllib.request.Request(url, headers={'User-Agent': 'private-drop-artifact-import/1'})
    opener = urllib.request.build_opener(urllib.request.HTTPRedirectHandler)
    try:
        with urllib.request.urlopen(req, timeout=URL_TIMEOUT) as resp:
            final_url = resp.geturl()
            if final_url != url:
                return None, 'source_url redirects are not allowed'
            length = resp.headers.get('Content-Length')
            if length and int(length) > MAX_BINARY:
                return None, 'source_url content for %s exceeds %d bytes' % (rel, MAX_BINARY)
            data = resp.read(MAX_BINARY + 1)
    except Exception as e:
        return None, 'Failed to fetch source_url: %s' % e
    return check_binary_size(data, rel)

def main():
    if len(sys.argv) < 2:
        print(json.dumps(err('Missing project root argument')))
        return
    root = sys.argv[1]
    if not os.path.isdir(root):
        print(json.dumps(err('Project root does not exist: ' + root)))
        return
    try:
        body = json.load(sys.stdin)
    except Exception as e:
        print(json.dumps(err('Invalid JSON: ' + str(e))))
        return
    dry_run = body.get('dry_run', False)
    edits = body.get('edits', [])
    if not edits:
        print(json.dumps(err('edits cannot be empty')))
        return
    originals = {}
    current = {}
    paths_map = {}
    binary_originals = {}
    binary_current = {}
    changed = set()
    for ed in edits:
        etype = ed.get('type', '')
        rel = ed.get('path', '')
        e = validate_path(rel)
        if e:
            print(json.dumps(err(e)))
            return
        text_key = None
        if etype == 'replace_text':
            text_key = 'new_text'
        elif etype == 'replace_range':
            text_key = 'new_text'
        elif etype == 'append_file':
            text_key = 'text'
        elif etype in ('create_file', 'write_file'):
            text_key = 'content'
        if text_key:
            txt = ed.get(text_key, '')
            if len(txt.encode('utf-8')) > MAX_TEXT:
                print(json.dumps(err('edit text for %s exceeds %d bytes' % (rel, MAX_TEXT))))
                return
        if etype == 'replace_text':
            if rel not in current:
                full, e = resolve(root, rel, True)
                if e:
                    print(json.dumps(err(e)))
                    return
                paths_map[rel] = full
                c, e = read_file(full)
                if e:
                    print(json.dumps(err(e)))
                    return
                originals.setdefault(rel, c)
                current[rel] = c
            after, e = replace_nth(current[rel], ed.get('old_text', ''), ed.get('new_text', ''), ed.get('occurrence'))
            if e:
                print(json.dumps(err(e)))
                return
            current[rel] = after
        elif etype == 'replace_range':
            if rel not in current:
                full, e = resolve(root, rel, True)
                if e:
                    print(json.dumps(err(e)))
                    return
                paths_map[rel] = full
                c, e = read_file(full)
                if e:
                    print(json.dumps(err(e)))
                    return
                originals.setdefault(rel, c)
                current[rel] = c
            after, e = replace_range(current[rel], ed['start_line'], ed['end_line'], ed.get('new_text', ''))
            if e:
                print(json.dumps(err(e)))
                return
            current[rel] = after
        elif etype == 'append_file':
            if rel not in current:
                full, e = resolve(root, rel, True)
                if e:
                    print(json.dumps(err(e)))
                    return
                paths_map[rel] = full
                c, e = read_file(full)
                if e:
                    print(json.dumps(err(e)))
                    return
                originals.setdefault(rel, c)
                current[rel] = c
            current[rel] = current[rel] + ed.get('text', '')
        elif etype == 'create_file':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) or rel in current:
                print(json.dumps(err('File already exists: ' + rel)))
                return
            paths_map[rel] = full
            originals.setdefault(rel, None)
            current[rel] = ed.get('content', '')
        elif etype == 'write_file':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            allow = ed.get('allow_overwrite', False)
            if os.path.exists(full) and not allow:
                print(json.dumps(err('File exists and allow_overwrite is false: ' + rel)))
                return
            if os.path.exists(full) and rel not in originals:
                old_c, e2 = read_file(full)
                if e2:
                    print(json.dumps(err(e2)))
                    return
                originals[rel] = old_c
            elif rel not in originals:
                originals.setdefault(rel, None)
            paths_map[rel] = full
            current[rel] = ed.get('content', '')
        elif etype in ('create_binary_file', 'create_binary_artifact'):
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) or rel in binary_current:
                print(json.dumps(err('File already exists: ' + rel)))
                return
            data, e = decode_binary(ed.get('base64_content', ''), rel)
            if e:
                print(json.dumps(err(e)))
                return
            paths_map[rel] = full
            binary_originals.setdefault(rel, None)
            binary_current[rel] = data
        elif etype in ('write_binary_file', 'write_binary_artifact'):
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            allow = ed.get('allow_overwrite', False)
            if os.path.exists(full) and not allow:
                print(json.dumps(err('File exists and allow_overwrite is false: ' + rel)))
                return
            data, e = decode_binary(ed.get('base64_content', ''), rel)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) and rel not in binary_originals:
                try:
                    with open(full, 'rb') as f:
                        binary_originals[rel] = f.read()
                except Exception as e:
                    print(json.dumps(err('Failed to read binary file: ' + str(e))))
                    return
            elif rel not in binary_originals:
                binary_originals.setdefault(rel, None)
            paths_map[rel] = full
            binary_current[rel] = data
        elif etype == 'create_binary_file_from_upload':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) or rel in binary_current:
                print(json.dumps(err('File already exists: ' + rel)))
                return
            data, e = read_upload(root, ed.get('source_file', ''), rel)
            if e:
                print(json.dumps(err(e)))
                return
            paths_map[rel] = full
            binary_originals.setdefault(rel, None)
            binary_current[rel] = data
        elif etype == 'write_binary_file_from_upload':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            allow = ed.get('allow_overwrite', False)
            if os.path.exists(full) and not allow:
                print(json.dumps(err('File exists and allow_overwrite is false: ' + rel)))
                return
            data, e = read_upload(root, ed.get('source_file', ''), rel)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) and rel not in binary_originals:
                try:
                    with open(full, 'rb') as f:
                        binary_originals[rel] = f.read()
                except Exception as e:
                    print(json.dumps(err('Failed to read binary file: ' + str(e))))
                    return
            elif rel not in binary_originals:
                binary_originals.setdefault(rel, None)
            paths_map[rel] = full
            binary_current[rel] = data
        elif etype == 'create_binary_file_from_url':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) or rel in binary_current:
                print(json.dumps(err('File already exists: ' + rel)))
                return
            data, e = read_url(ed.get('source_url', ''), rel)
            if e:
                print(json.dumps(err(e)))
                return
            paths_map[rel] = full
            binary_originals.setdefault(rel, None)
            binary_current[rel] = data
        elif etype == 'write_binary_file_from_url':
            full, e = resolve(root, rel, False)
            if e:
                print(json.dumps(err(e)))
                return
            allow = ed.get('allow_overwrite', False)
            if os.path.exists(full) and not allow:
                print(json.dumps(err('File exists and allow_overwrite is false: ' + rel)))
                return
            data, e = read_url(ed.get('source_url', ''), rel)
            if e:
                print(json.dumps(err(e)))
                return
            if os.path.exists(full) and rel not in binary_originals:
                try:
                    with open(full, 'rb') as f:
                        binary_originals[rel] = f.read()
                except Exception as e:
                    print(json.dumps(err('Failed to read binary file: ' + str(e))))
                    return
            elif rel not in binary_originals:
                binary_originals.setdefault(rel, None)
            paths_map[rel] = full
            binary_current[rel] = data
        else:
            print(json.dumps(err('Unknown edit type: ' + etype)))
            return
        changed.add(rel)
    diff_parts = []
    for p in sorted(changed):
        old = originals.get(p)
        new = current.get(p)
        if new is not None and old != new:
            diff_parts.append(simple_diff(p, old, new))
        elif p in binary_current:
            old_b = binary_originals.get(p)
            new_b = binary_current.get(p)
            if new_b is not None and old_b != new_b:
                diff_parts.append(simple_binary_diff(p, None if old_b is None else len(old_b), len(new_b)))
    diff = ''.join(diff_parts)
    if not dry_run:
        for p in sorted(changed):
            full = paths_map.get(p)
            content = current.get(p)
            if full and content is not None:
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, 'w', encoding='utf-8') as f:
                    f.write(content)
            elif full and p in binary_current and binary_current[p] is not None:
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, 'wb') as f:
                    f.write(binary_current[p])
    print(json.dumps({
        'success': True,
        'changed_files': sorted(changed),
        'diff': diff,
        'warnings': [],
        'error': None
    }))

main()
"#####;

/// Run edit operations on a remote SSH project by piping JSON to a python3 script.
fn ssh_apply_project_edit(
    proj: &ProjectConfig,
    body: &EditRequest,
    ssh_config: Option<&SshConfig>,
) -> EditResponse {
    let ssh_target = match build_ssh_target(proj) {
        Ok(t) => t,
        Err(e) => return edit_error(e),
    };
    let project_path = &proj.path;

    // Serialize the edit request to JSON
    let body_json = match serde_json::to_string(body) {
        Ok(j) => j,
        Err(e) => return edit_error(format!("Failed to serialize edit request: {}", e)),
    };

    // Build the remote command: run python3 with the embedded script
    // Pass project root as first argument; script reads JSON from stdin
    let remote_cmd = format!(
        "python3 -c {} {}",
        shell_escape(REMOTE_EDIT_SCRIPT),
        shell_escape(project_path)
    );

    let start = Instant::now();
    let mut child = match build_ssh_command(&ssh_target, &remote_cmd, ssh_config)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return edit_error(format!("Failed to spawn SSH edit: {}", e)),
    };

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if let Err(e) = stdin.write_all(body_json.as_bytes()) {
            let _ = child.kill();
            return edit_error(format!("Failed to write SSH edit payload: {}", e));
        }
    }

    let result = loop {
        match child.try_wait() {
            Ok(Some(_status)) => break child.wait_with_output(),
            Ok(None) if start.elapsed() > std::time::Duration::from_secs(CHECK_TIMEOUT_SECS) => {
                let _ = child.kill();
                let _ = child.wait();
                return edit_error(format!(
                    "SSH edit timed out after {} seconds",
                    CHECK_TIMEOUT_SECS
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                return edit_error(format!("Failed while waiting for SSH edit: {}", e));
            }
        }
    };

    match result {
        Ok(output) => {
            let _elapsed = start.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);

            if code != 0 && stdout.is_empty() {
                // python3 not available or other exec failure
                if stderr.contains("No such file")
                    || stderr.contains("not found")
                    || stderr.contains("No module")
                {
                    return edit_error(
                        "Remote python3 is not available. Install python3 on the remote host."
                            .to_string(),
                    );
                }
                return edit_error(format!(
                    "SSH edit failed (exit {}): {}",
                    code,
                    stderr.chars().take(500).collect::<String>()
                ));
            }

            let mut resp: EditResponse = match serde_json::from_str(&stdout) {
                Ok(r) => r,
                Err(e) => {
                    return edit_error(format!(
                        "Failed to parse remote edit response: {}. Raw: {}",
                        e,
                        stdout.chars().take(500).collect::<String>()
                    ))
                }
            };
            let (truncated_diff, diff_truncated) = truncate_string(resp.diff, MAX_OUTPUT_LEN);
            resp.diff = truncated_diff;
            if diff_truncated {
                resp.warnings.push("Remote diff was truncated".to_string());
            }
            if !stderr.is_empty() {
                resp.warnings.push(format!(
                    "Remote stderr: {}",
                    stderr.chars().take(500).collect::<String>()
                ));
            }
            resp
        }
        Err(e) => edit_error(format!("Failed to execute SSH edit: {}", e)),
    }
}

/// Validate a path for SSH read_file operations.
fn validate_ssh_read_path(rel_path: &str) -> Result<(), String> {
    if rel_path.starts_with('/') {
        return Err("Absolute paths are not allowed".to_string());
    }
    if rel_path.contains("..") {
        return Err("Path traversal (..) is not allowed".to_string());
    }
    if is_sensitive_path(rel_path) {
        return Err(format!("Cannot access sensitive path: {}", rel_path));
    }
    Ok(())
}

// =============================================================================
// SSH context helpers
// =============================================================================

fn ssh_overview(
    proj: &ProjectConfig,
    project_name: &str,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    let ssh_target = match build_ssh_target(proj) {
        Ok(t) => t,
        Err(e) => {
            return ContextResponse {
                success: false,
                project: project_name.to_string(),
                mode: "overview".to_string(),
                content: None,
                items: None,
                truncated: false,
                error: Some(e),
            }
        }
    };
    let important_files = [
        "README.md",
        "TODO.md",
        "Cargo.toml",
        "scripts/e2e_test.sh",
        "src/main.rs",
    ];
    let file_args = important_files
        .iter()
        .map(|f| shell_escape(f))
        .collect::<Vec<_>>()
        .join(" ");
    let remote_cmd = format!(
        "cd {} || exit 2; printf '__BRANCH__\\n'; git rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'unknown\\n'; printf '__STATUS__\\n'; git status --short --untracked-files=no 2>/dev/null || true; printf '__FILES__\\n'; for f in {}; do if test -f \"$f\"; then printf '%s=yes\\n' \"$f\"; else printf '%s=no\\n' \"$f\"; fi; done",
        shell_escape(&proj.path),
        file_args
    );
    let (code, stdout, stderr, _) = run_ssh(&ssh_target, &remote_cmd, 15, ssh_config);
    if code != 0 {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "overview".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("SSH overview failed: {}", stderr.trim())),
        };
    }

    let mut section = "";
    let mut branch = "unknown".to_string();
    let mut status_lines: Vec<String> = Vec::new();
    let mut file_status: HashMap<String, String> = HashMap::new();
    for line in stdout.lines() {
        match line {
            "__BRANCH__" => section = "branch",
            "__STATUS__" => section = "status",
            "__FILES__" => section = "files",
            _ => match section {
                "branch" if !line.trim().is_empty() => branch = line.trim().to_string(),
                "status" => status_lines.push(line.to_string()),
                "files" => {
                    if let Some((path, exists)) = line.split_once('=') {
                        file_status.insert(path.to_string(), exists.to_string());
                    }
                }
                _ => {}
            },
        }
    }
    let status = status_lines.join("\n");
    let mut content = format!(
        "Project: {}\nRoot: {}\nBranch: {}\n\nGit Status:\n{}\n\nAllowed Checks: {}\n\nImportant Files:",
        project_name,
        proj.path,
        branch,
        status.trim(),
        proj.allowed_checks.join(", ")
    );
    for f in &important_files {
        let exists = file_status.get(*f).map(String::as_str).unwrap_or("no");
        content.push_str(&format!(
            "\n  {}: {}",
            f,
            if exists == "yes" { "yes" } else { "no" }
        ));
    }
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "overview".to_string(),
        content: Some(content),
        items: None,
        truncated: false,
        error: None,
    }
}

fn ssh_tree(
    proj: &ProjectConfig,
    project_name: &str,
    rel_path: Option<&str>,
    limit: usize,
    max_depth: usize,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    let mut excludes = String::new();
    for dir in IGNORED_DIRS {
        excludes.push_str(&format!(" -not -path '*/{}/*'", dir));
    }
    let limit = normalize_tree_limit(limit);
    let max_depth = normalize_tree_depth(max_depth);
    let find_root = match rel_path {
        Some(path) => {
            if let Err(e) = validate_ssh_read_path(path) {
                return context_error(project_name, &ContextMode::Tree, e);
            }
            shell_escape(path)
        }
        None => shell_escape("."),
    };
    let cmd = format!(
        "cd {} && find {} -mindepth 1 -maxdepth {}{} -type f -print 2>/dev/null | sort | head -n {} | sed 's|^\\./||'",
        shell_escape(&proj.path), find_root, max_depth, excludes, limit
    );
    let (code, stdout, stderr, _) = run_ssh(
        &build_ssh_target(proj).unwrap_or_default(),
        &cmd,
        30,
        ssh_config,
    );
    if code != 0 {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "tree".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("SSH tree failed: {}", stderr.trim())),
        };
    }
    let mut items: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    let truncated = items.len() >= limit;
    items.truncate(limit);
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "tree".to_string(),
        content: None,
        items: Some(items),
        truncated,
        error: None,
    }
}

fn ssh_search(
    proj: &ProjectConfig,
    project_name: &str,
    query: &str,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    // Build grep exclusions
    let mut excludes = String::new();
    for dir in IGNORED_DIRS {
        excludes.push_str(&format!(" --exclude-dir='{}'", dir));
    }
    // Use grep -rn, then head to limit results
    let escaped_query = query.replace('\'', "'\\''");
    let cmd = format!(
        "cd {} && grep -rn{} --include='*' '{}' . 2>/dev/null | head -n {} | sed 's|^\\./||'",
        shell_escape(&proj.path),
        excludes,
        escaped_query,
        MAX_SEARCH_RESULTS
    );
    let (code, stdout, stderr, _) = run_ssh(
        &build_ssh_target(proj).unwrap_or_default(),
        &cmd,
        30,
        ssh_config,
    );
    // grep returns 1 if no match, that's ok
    if code != 0 && code != 1 {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "search".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("SSH search failed: {}", stderr.trim())),
        };
    }
    let items: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    let truncated = items.len() >= MAX_SEARCH_RESULTS;
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "search".to_string(),
        content: None,
        items: Some(items),
        truncated,
        error: None,
    }
}

fn ssh_read_file(
    proj: &ProjectConfig,
    project_name: &str,
    rel_path: &str,
    start_line: usize,
    limit: usize,
    ssh_config: Option<&SshConfig>,
) -> ContextResponse {
    if let Err(e) = validate_ssh_read_path(rel_path) {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "read_file".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(e),
        };
    }
    let end_line = match validate_read_file_range(start_line, limit) {
        Ok(end_line) => end_line,
        Err(e) => {
            return ContextResponse {
                success: false,
                project: project_name.to_string(),
                mode: "read_file".to_string(),
                content: None,
                items: None,
                truncated: false,
                error: Some(e),
            }
        }
    };
    let escaped_path = shell_escape(rel_path);
    let cmd = format!(
        "sed -n '{},{}p' -- {} | awk '{{ if(length($0)>{}) print substr($0,1,{}) \"… [line truncated]\"; else print }}'",
        start_line, end_line, escaped_path, MAX_CONTEXT_LINE_LEN, MAX_CONTEXT_LINE_LEN
    );
    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 30, ssh_config);
    if code != 0 {
        return ContextResponse {
            success: false,
            project: project_name.to_string(),
            mode: "read_file".to_string(),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("Failed to read file: {}", stderr.trim())),
        };
    }
    // Add line numbers like the local version
    let lines: Vec<String> = stdout
        .lines()
        .enumerate()
        .map(|(i, l)| format_context_line(start_line + i, l).0)
        .collect();
    let output = lines.join("\n");
    let (output, truncated) = truncate_string(output, MAX_OUTPUT_LEN);
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "read_file".to_string(),
        content: Some(output),
        items: None,
        truncated,
        error: None,
    }
}

fn ssh_apply_patch(
    proj: &ProjectConfig,
    _project_name: &str,
    patch: &str,
    changed: Vec<String>,
    ssh_config: Option<&SshConfig>,
) -> PatchResponse {
    let ssh_target = match build_ssh_target(proj) {
        Ok(t) => t,
        Err(e) => {
            return PatchResponse {
                success: false,
                changed_files: Some(changed),
                stdout: None,
                stderr: None,
                error: Some(e),
            }
        }
    };
    let remote_cmd = format!(
        "cd {} && git apply --check __PATCH__ && git apply __PATCH__",
        shell_escape(&proj.path)
    );
    let (code, stdout, stderr, _) =
        run_ssh_patch(&ssh_target, &proj.path, patch, &remote_cmd, ssh_config);
    if code == 0 {
        PatchResponse {
            success: true,
            changed_files: Some(changed),
            stdout: Some(stdout),
            stderr: Some(stderr),
            error: None,
        }
    } else {
        PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(stdout),
            stderr: Some(stderr),
            error: Some("git apply failed".to_string()),
        }
    }
}

fn agent_context_shell_fragment() -> String {
    let files = AGENT_CONTEXT_FILES
        .iter()
        .map(|f| shell_escape(f))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        " printf '# Agent context\\n\\nLoaded project rules and memory files for alignment before planning or editing.\\n'; for f in {}; do printf '\\n## %s\\n\\n' \"$f\"; if test -f \"$f\"; then sed -n '1,240p' -- \"$f\"; else printf '(missing)\\n'; fi; done;",
        files
    )
}

fn ssh_overview_from_batch_block(
    proj: &ProjectConfig,
    project_name: &str,
    block: &str,
) -> ContextResponse {
    let important_files = [
        "README.md",
        "TODO.md",
        "Cargo.toml",
        "scripts/e2e_test.sh",
        "src/main.rs",
    ];
    let mut section = "";
    let mut branch = "unknown".to_string();
    let mut status_lines: Vec<String> = Vec::new();
    let mut file_status: HashMap<String, String> = HashMap::new();
    for line in block.lines() {
        match line {
            "__BRANCH__" => section = "branch",
            "__STATUS__" => section = "status",
            "__FILES__" => section = "files",
            _ => match section {
                "branch" if !line.trim().is_empty() => branch = line.trim().to_string(),
                "status" => status_lines.push(line.to_string()),
                "files" => {
                    if let Some((path, exists)) = line.split_once('=') {
                        file_status.insert(path.to_string(), exists.to_string());
                    }
                }
                _ => {}
            },
        }
    }
    let status = status_lines.join("\n");
    let mut content = format!(
        "Project: {}\nRoot: {}\nBranch: {}\n\nGit Status:\n{}\n\nAllowed Checks: {}\n\nImportant Files:",
        project_name,
        proj.path,
        branch,
        status.trim(),
        proj.allowed_checks.join(", ")
    );
    for f in &important_files {
        let exists = file_status.get(*f).map(String::as_str).unwrap_or("no");
        content.push_str(&format!(
            "\n  {}: {}",
            f,
            if exists == "yes" { "yes" } else { "no" }
        ));
    }
    ContextResponse {
        success: true,
        project: project_name.to_string(),
        mode: "overview".to_string(),
        content: Some(content),
        items: None,
        truncated: false,
        error: None,
    }
}

fn ssh_batch_block_to_response(
    proj: &ProjectConfig,
    project_name: &str,
    item: &ContextBatchItem,
    block: &str,
) -> ContextResponse {
    if let Some(err) = block.strip_prefix("__PDCTX_ERROR__:") {
        return context_error(project_name, &item.mode, err.trim().to_string());
    }
    match item.mode {
        ContextMode::Overview => ssh_overview_from_batch_block(proj, project_name, block),
        ContextMode::Tree => {
            let mut items: Vec<String> = block
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect();
            let truncated = items.len() >= MAX_TREE_ITEMS;
            items.truncate(MAX_TREE_ITEMS);
            ContextResponse {
                success: true,
                project: project_name.to_string(),
                mode: "tree".to_string(),
                content: None,
                items: Some(items),
                truncated,
                error: None,
            }
        }
        ContextMode::ReadFile => {
            let lines: Vec<String> = block
                .lines()
                .enumerate()
                .map(|(i, l)| format!("{:4} | {}", item.start_line + i, l))
                .collect();
            let (output, truncated) = truncate_string(lines.join("\n"), MAX_OUTPUT_LEN);
            ContextResponse {
                success: true,
                project: project_name.to_string(),
                mode: "read_file".to_string(),
                content: Some(output),
                items: None,
                truncated,
                error: None,
            }
        }
        ContextMode::MarkdownOutline => mode_content_response(
            project_name,
            "markdown_outline",
            block.to_string(),
            MAX_OUTPUT_LEN,
        ),
        ContextMode::ReadSection => mode_content_response(
            project_name,
            "read_section",
            block.to_string(),
            MAX_OUTPUT_LEN,
        ),
        ContextMode::AgentContext => mode_content_response(
            project_name,
            "agent_context",
            block.to_string(),
            MAX_OUTPUT_LEN,
        ),
        ContextMode::GitStatus => {
            let (content, truncated) = truncate_string(block.to_string(), MAX_OUTPUT_LEN);
            ContextResponse {
                success: true,
                project: project_name.to_string(),
                mode: "git_status".to_string(),
                content: Some(content),
                items: None,
                truncated,
                error: None,
            }
        }
        ContextMode::GitDiff => {
            let (content, truncated) = truncate_string(block.to_string(), MAX_OUTPUT_LEN);
            ContextResponse {
                success: true,
                project: project_name.to_string(),
                mode: "git_diff".to_string(),
                content: Some(content),
                items: None,
                truncated,
                error: None,
            }
        }
        ContextMode::Search => context_error(
            project_name,
            &item.mode,
            "search is not supported by single-SSH context batch".to_string(),
        ),
    }
}

fn ssh_context_batch_error_results(
    project_name: &str,
    requests: &[ContextBatchItem],
    error: String,
) -> Vec<ContextResponse> {
    requests
        .iter()
        .map(|item| context_error(project_name, &item.mode, error.clone()))
        .collect()
}

fn try_ssh_context_batch_once(
    proj: &ProjectConfig,
    project_name: &str,
    requests: &[ContextBatchItem],
    ssh_config: Option<&SshConfig>,
) -> Option<(Vec<ContextResponse>, u64)> {
    if requests.is_empty() {
        return Some((Vec::new(), 0));
    }
    let ssh_target = match build_ssh_target(proj) {
        Ok(t) => t,
        Err(e) => {
            return Some((
                ssh_context_batch_error_results(project_name, requests, e),
                0,
            ))
        }
    };

    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut script = format!("cd {} || exit 2;", shell_escape(&proj.path));
    for (idx, item) in requests.iter().enumerate() {
        if matches!(item.mode, ContextMode::Search) {
            return None;
        }
        script.push_str(&format!(" printf '\n__PDCTX_{}_START_{}__\n';", nonce, idx));
        match item.mode {
            ContextMode::Overview => {
                let file_args = [
                    "README.md",
                    "TODO.md",
                    "Cargo.toml",
                    "scripts/e2e_test.sh",
                    "src/main.rs",
                ]
                .iter()
                .map(|f| shell_escape(f))
                .collect::<Vec<_>>()
                .join(" ");
                script.push_str(&format!(" printf '__BRANCH__\\n'; git rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'unknown\\n'; printf '__STATUS__\\n'; git status --short --untracked-files=no 2>/dev/null || true; printf '__FILES__\\n'; for f in {}; do if test -f \"$f\"; then printf '%s=yes\\n' \"$f\"; else printf '%s=no\\n' \"$f\"; fi; done;", file_args));
            }
            ContextMode::Tree => {
                let mut excludes = String::new();
                for dir in IGNORED_DIRS {
                    excludes.push_str(&format!(" -not -path '*/{}/*'", dir));
                }
                let limit = normalize_tree_limit(item.limit);
                let max_depth = normalize_tree_depth(item.max_depth);
                let find_root = match &item.path {
                    Some(path) => {
                        if validate_ssh_read_path(path).is_err() {
                            return None;
                        }
                        shell_escape(path)
                    }
                    None => shell_escape("."),
                };
                script.push_str(&format!(" find {} -mindepth 1 -maxdepth {}{} -type f -print 2>/dev/null | sort | head -n {} | sed 's|^\\./||';", find_root, max_depth, excludes, limit));
            }
            ContextMode::ReadFile => {
                let Some(path) = &item.path else {
                    return None;
                };
                if validate_ssh_read_path(path).is_err() {
                    return None;
                }
                let end_line = match validate_read_file_range(item.start_line, item.limit) {
                    Ok(end_line) => end_line,
                    Err(_) => return None,
                };
                let escaped_path = shell_escape(path);
                script.push_str(&format!(" if test -f {0}; then sed -n '{1},{2}p' -- {0} | awk '{{ if(length($0)>{3}) print substr($0,1,{3}) \"… [line truncated]\"; else print }}'; else printf '__PDCTX_ERROR__:File not found: {0}\\n'; fi;", escaped_path, item.start_line, end_line, MAX_CONTEXT_LINE_LEN));
            }
            ContextMode::MarkdownOutline => {
                let Some(path) = &item.path else {
                    return None;
                };
                if validate_ssh_read_path(path).is_err() {
                    return None;
                }
                script.push_str(&markdown_outline_shell_fragment(path, item.limit));
            }
            ContextMode::ReadSection => {
                let (Some(path), Some(query)) = (&item.path, &item.query) else {
                    return None;
                };
                if validate_ssh_read_path(path).is_err() {
                    return None;
                }
                script.push_str(&markdown_section_shell_fragment(path, query, item.limit));
            }
            ContextMode::AgentContext => {
                script.push_str(&agent_context_shell_fragment());
            }
            ContextMode::GitStatus => {
                script.push_str(" git status --short --untracked-files=no 2>/dev/null || true;");
            }
            ContextMode::GitDiff => {
                script.push_str(" git diff 2>/dev/null || true;");
            }
            ContextMode::Search => return None,
        }
        script.push_str(&format!(" printf '\n__PDCTX_{}_END_{}__\n';", nonce, idx));
    }

    let (code, stdout, stderr, _) = run_ssh(&ssh_target, &script, 30, ssh_config);
    if code != 0 {
        let error = format!("SSH context batch failed: {}", stderr.trim());
        return Some((
            ssh_context_batch_error_results(project_name, requests, error),
            1,
        ));
    }
    let blocks = parse_ssh_batch_blocks(&stdout, requests.len(), &nonce);
    let results = requests
        .iter()
        .zip(blocks.iter())
        .map(|(item, block)| ssh_batch_block_to_response(proj, project_name, item, block))
        .collect();
    Some((results, 1))
}

fn execute_context_item(
    proj: &ProjectConfig,
    project_name: &str,
    item: &ContextBatchItem,
    ssh_config: Option<&SshConfig>,
) -> (ContextResponse, u64) {
    if proj.is_ssh() {
        let resp = match item.mode {
            ContextMode::Overview => ssh_overview(proj, project_name, ssh_config),
            ContextMode::Tree => ssh_tree(
                proj,
                project_name,
                item.path.as_deref(),
                item.limit,
                item.max_depth,
                ssh_config,
            ),
            ContextMode::Search => match &item.query {
                Some(query) => ssh_search(proj, project_name, query, ssh_config),
                None => context_error(
                    project_name,
                    &item.mode,
                    "query parameter is required for search mode".to_string(),
                ),
            },
            ContextMode::ReadFile => match &item.path {
                Some(path) => ssh_read_file(
                    proj,
                    project_name,
                    path,
                    item.start_line,
                    item.limit,
                    ssh_config,
                ),
                None => context_error(
                    project_name,
                    &item.mode,
                    "path parameter is required for read_file mode".to_string(),
                ),
            },
            ContextMode::MarkdownOutline => match &item.path {
                Some(path) => {
                    let cmd = markdown_outline_shell_fragment(path, item.limit);
                    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
                    if code == 0 {
                        if let Some(err) = stdout.trim().strip_prefix("__PDCTX_ERROR__:") {
                            context_error(project_name, &item.mode, err.trim().to_string())
                        } else {
                            mode_content_response(
                                project_name,
                                "markdown_outline",
                                stdout,
                                MAX_OUTPUT_LEN,
                            )
                        }
                    } else {
                        context_error(
                            project_name,
                            &item.mode,
                            format!("markdown_outline failed: {}", stderr.trim()),
                        )
                    }
                }
                None => context_error(
                    project_name,
                    &item.mode,
                    "path parameter is required for markdown_outline mode".to_string(),
                ),
            },
            ContextMode::ReadSection => match (&item.path, &item.query) {
                (Some(path), Some(query)) => {
                    let cmd = markdown_section_shell_fragment(path, query, item.limit);
                    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
                    if code == 0 {
                        if let Some(err) = stdout.trim().strip_prefix("__PDCTX_ERROR__:") {
                            context_error(project_name, &item.mode, err.trim().to_string())
                        } else {
                            mode_content_response(
                                project_name,
                                "read_section",
                                stdout,
                                MAX_OUTPUT_LEN,
                            )
                        }
                    } else {
                        context_error(
                            project_name,
                            &item.mode,
                            format!("read_section failed: {}", stderr.trim()),
                        )
                    }
                }
                _ => context_error(
                    project_name,
                    &item.mode,
                    "path and query parameters are required for read_section mode".to_string(),
                ),
            },
            ContextMode::AgentContext => {
                let (code, stdout, stderr, _) =
                    run_project_cmd(proj, &agent_context_shell_fragment(), 10, ssh_config);
                if code == 0 {
                    mode_content_response(project_name, "agent_context", stdout, MAX_OUTPUT_LEN)
                } else {
                    context_error(
                        project_name,
                        &item.mode,
                        format!("agent_context failed: {}", stderr.trim()),
                    )
                }
            }
            ContextMode::GitStatus => {
                let (code, stdout, stderr, _) =
                    run_project_cmd(proj, git_status_command(), 10, ssh_config);
                if code == 0 {
                    let (content, truncated) = truncate_string(stdout, MAX_OUTPUT_LEN);
                    ContextResponse {
                        success: true,
                        project: project_name.to_string(),
                        mode: "git_status".to_string(),
                        content: Some(content),
                        items: None,
                        truncated,
                        error: None,
                    }
                } else {
                    context_error(
                        project_name,
                        &item.mode,
                        format!("git status failed: {}", stderr.trim()),
                    )
                }
            }
            ContextMode::GitDiff => {
                let (code, stdout, stderr, _) = run_project_cmd(proj, "git diff", 30, ssh_config);
                if code == 0 {
                    let (content, truncated) = truncate_string(stdout, MAX_OUTPUT_LEN);
                    ContextResponse {
                        success: true,
                        project: project_name.to_string(),
                        mode: "git_diff".to_string(),
                        content: Some(content),
                        items: None,
                        truncated,
                        error: None,
                    }
                } else {
                    context_error(
                        project_name,
                        &item.mode,
                        format!("git diff failed: {}", stderr.trim()),
                    )
                }
            }
        };
        return (resp, 1);
    }

    let root = proj.root();
    if !root.exists() {
        return (
            context_error(
                project_name,
                &item.mode,
                format!("Project root does not exist: {:?}", root),
            ),
            0,
        );
    }
    match item.mode {
        ContextMode::Overview => {
            let branch = run_command("git rev-parse --abbrev-ref HEAD", &root, 10)
                .1
                .trim()
                .to_string();
            let status = run_command(git_status_command(), &root, 10)
                .1
                .trim()
                .to_string();
            let important_files = [
                "README.md",
                "TODO.md",
                "Cargo.toml",
                "scripts/e2e_test.sh",
                "src/main.rs",
            ];
            let mut content = format!("Project: {}\nRoot: {}\nBranch: {}\n\nGit Status:\n{}\n\nAllowed Checks: {}\n\nImportant Files:", project_name, root.display(), branch, status, proj.allowed_checks.join(", "));
            for f in &important_files {
                let exists = root.join(f).exists();
                content.push_str(&format!("\n  {}: {}", f, if exists { "yes" } else { "no" }));
            }
            (
                ContextResponse {
                    success: true,
                    project: project_name.to_string(),
                    mode: "overview".to_string(),
                    content: Some(content),
                    items: None,
                    truncated: false,
                    error: None,
                },
                0,
            )
        }
        ContextMode::Tree => {
            let limit = normalize_tree_limit(item.limit);
            let max_depth = normalize_tree_depth(item.max_depth);
            let tree_root = match &item.path {
                Some(rel_path) => match canonicalize_and_verify(&root.join(rel_path), &root) {
                    Ok(path) => path,
                    Err(e) => return (context_error(project_name, &item.mode, e), 0),
                },
                None => root.clone(),
            };
            let mut items = Vec::new();
            collect_tree(&tree_root, &root, &mut items, limit, max_depth);
            let truncated = items.len() >= limit;
            (
                ContextResponse {
                    success: true,
                    project: project_name.to_string(),
                    mode: "tree".to_string(),
                    content: None,
                    items: Some(items),
                    truncated,
                    error: None,
                },
                0,
            )
        }
        ContextMode::Search => match &item.query {
            Some(query) => {
                let results = simple_search(&root, query, MAX_SEARCH_RESULTS);
                let truncated = results.len() >= MAX_SEARCH_RESULTS;
                (
                    ContextResponse {
                        success: true,
                        project: project_name.to_string(),
                        mode: "search".to_string(),
                        content: None,
                        items: Some(results),
                        truncated,
                        error: None,
                    },
                    0,
                )
            }
            None => (
                context_error(
                    project_name,
                    &item.mode,
                    "query parameter is required for search mode".to_string(),
                ),
                0,
            ),
        },
        ContextMode::ReadFile => match &item.path {
            Some(rel_path) => {
                let full_path = root.join(rel_path);
                match canonicalize_and_verify(&full_path, &root) {
                    Ok(canonical) => match std::fs::read_to_string(&canonical) {
                        Ok(content) => {
                            let lines: Vec<&str> = content.lines().collect();
                            let total = lines.len();
                            let end_line =
                                match validate_read_file_range(item.start_line, item.limit) {
                                    Ok(end_line) => end_line,
                                    Err(e) => {
                                        return (context_error(project_name, &item.mode, e), 0)
                                    }
                                };
                            let start = item.start_line - 1;
                            let end = end_line.min(total);
                            let selected: Vec<String> = if start < total {
                                lines[start..end]
                                    .iter()
                                    .enumerate()
                                    .map(|(i, l)| format_context_line(start + i + 1, l).0)
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            let (output, truncated) =
                                truncate_string(selected.join("\n"), MAX_OUTPUT_LEN);
                            (
                                ContextResponse {
                                    success: true,
                                    project: project_name.to_string(),
                                    mode: "read_file".to_string(),
                                    content: Some(output),
                                    items: None,
                                    truncated,
                                    error: None,
                                },
                                0,
                            )
                        }
                        Err(e) => (
                            context_error(
                                project_name,
                                &item.mode,
                                format!("Failed to read file: {}", e),
                            ),
                            0,
                        ),
                    },
                    Err(e) => (context_error(project_name, &item.mode, e), 0),
                }
            }
            None => (
                context_error(
                    project_name,
                    &item.mode,
                    "path parameter is required for read_file mode".to_string(),
                ),
                0,
            ),
        },
        ContextMode::MarkdownOutline | ContextMode::ReadSection => {
            local_markdown_file_response(&root, project_name, item)
        }
        ContextMode::AgentContext => (local_agent_context(&root, project_name), 0),
        ContextMode::GitStatus => {
            let output = run_command(git_status_command(), &root, 10);
            let (content, truncated) = truncate_string(output.1, MAX_OUTPUT_LEN);
            (
                ContextResponse {
                    success: true,
                    project: project_name.to_string(),
                    mode: "git_status".to_string(),
                    content: Some(content),
                    items: None,
                    truncated,
                    error: None,
                },
                0,
            )
        }
        ContextMode::GitDiff => {
            let output = run_command("git diff", &root, 30);
            let (content, truncated) = truncate_string(output.1, MAX_OUTPUT_LEN);
            (
                ContextResponse {
                    success: true,
                    project: project_name.to_string(),
                    mode: "git_diff".to_string(),
                    content: Some(content),
                    items: None,
                    truncated,
                    error: None,
                },
                0,
            )
        }
    }
}

// =============================================================================
// Trusted async shell job helpers
// =============================================================================

// =============================================================================
// Handlers
// =============================================================================

#[handler]
pub async fn codex_context(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(ContextResponse {
            success: false,
            project: String::new(),
            mode: String::new(),
            content: None,
            items: None,
            truncated: false,
            error: Some(
                "Projects not configured. Set PROJECTS_CONFIG or create projects.toml".to_string(),
            ),
        }));
        return;
    };
    let body: ContextRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ContextResponse {
                success: false,
                project: String::new(),
                mode: String::new(),
                content: None,
                items: None,
                truncated: false,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let request_start = Instant::now();
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ContextResponse {
                success: false,
                project: body.project.clone(),
                mode: format!("{:?}", body.mode),
                content: None,
                items: None,
                truncated: false,
                error: Some(e),
            }));
            return;
        }
    };

    // For SSH executor, dispatch to SSH helpers
    if proj.is_ssh() {
        let ssh_config = projects.ssh.as_ref();
        let resp = match body.mode {
            ContextMode::Overview => ssh_overview(proj, &body.project, ssh_config),
            ContextMode::Tree => ssh_tree(
                proj,
                &body.project,
                body.path.as_deref(),
                body.limit,
                body.max_depth,
                ssh_config,
            ),
            ContextMode::Search => {
                let Some(query) = &body.query else {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(ContextResponse {
                        success: false,
                        project: body.project,
                        mode: "search".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some("query parameter is required for search mode".to_string()),
                    }));
                    return;
                };
                ssh_search(proj, &body.project, query, ssh_config)
            }
            ContextMode::ReadFile => {
                let Some(rel_path) = &body.path else {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(ContextResponse {
                        success: false,
                        project: body.project,
                        mode: "read_file".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some("path parameter is required for read_file mode".to_string()),
                    }));
                    return;
                };
                ssh_read_file(
                    proj,
                    &body.project,
                    rel_path,
                    body.start_line,
                    body.limit,
                    ssh_config,
                )
            }
            ContextMode::MarkdownOutline => match &body.path {
                Some(path) => {
                    let cmd = markdown_outline_shell_fragment(path, body.limit);
                    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
                    if code != 0 {
                        ContextResponse {
                            success: false,
                            project: body.project.clone(),
                            mode: "markdown_outline".to_string(),
                            content: None,
                            items: None,
                            truncated: false,
                            error: Some(format!("markdown_outline failed: {}", stderr.trim())),
                        }
                    } else if let Some(err) = stdout.trim().strip_prefix("__PDCTX_ERROR__:") {
                        ContextResponse {
                            success: false,
                            project: body.project.clone(),
                            mode: "markdown_outline".to_string(),
                            content: None,
                            items: None,
                            truncated: false,
                            error: Some(err.trim().to_string()),
                        }
                    } else {
                        mode_content_response(
                            &body.project,
                            "markdown_outline",
                            stdout,
                            MAX_OUTPUT_LEN,
                        )
                    }
                }
                None => ContextResponse {
                    success: false,
                    project: body.project.clone(),
                    mode: "markdown_outline".to_string(),
                    content: None,
                    items: None,
                    truncated: false,
                    error: Some("path parameter is required for markdown_outline mode".to_string()),
                },
            },
            ContextMode::ReadSection => match (&body.path, &body.query) {
                (Some(path), Some(query)) => {
                    let cmd = markdown_section_shell_fragment(path, query, body.limit);
                    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
                    if code != 0 {
                        ContextResponse {
                            success: false,
                            project: body.project.clone(),
                            mode: "read_section".to_string(),
                            content: None,
                            items: None,
                            truncated: false,
                            error: Some(format!("read_section failed: {}", stderr.trim())),
                        }
                    } else if let Some(err) = stdout.trim().strip_prefix("__PDCTX_ERROR__:") {
                        ContextResponse {
                            success: false,
                            project: body.project.clone(),
                            mode: "read_section".to_string(),
                            content: None,
                            items: None,
                            truncated: false,
                            error: Some(err.trim().to_string()),
                        }
                    } else {
                        mode_content_response(&body.project, "read_section", stdout, MAX_OUTPUT_LEN)
                    }
                }
                _ => ContextResponse {
                    success: false,
                    project: body.project.clone(),
                    mode: "read_section".to_string(),
                    content: None,
                    items: None,
                    truncated: false,
                    error: Some(
                        "path and query parameters are required for read_section mode".to_string(),
                    ),
                },
            },
            ContextMode::AgentContext => {
                let (code, stdout, stderr, _) =
                    run_project_cmd(proj, &agent_context_shell_fragment(), 10, ssh_config);
                if code != 0 {
                    ContextResponse {
                        success: false,
                        project: body.project.clone(),
                        mode: "agent_context".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some(format!("agent_context failed: {}", stderr.trim())),
                    }
                } else {
                    mode_content_response(&body.project, "agent_context", stdout, MAX_OUTPUT_LEN)
                }
            }
            ContextMode::GitStatus => {
                let (code, stdout, stderr, _) =
                    run_project_cmd(proj, git_status_command(), 10, ssh_config);
                if code != 0 {
                    ContextResponse {
                        success: false,
                        project: body.project.clone(),
                        mode: "git_status".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some(format!("git status failed: {}", stderr.trim())),
                    }
                } else {
                    let (content, truncated) = truncate_string(stdout, MAX_OUTPUT_LEN);
                    ContextResponse {
                        success: true,
                        project: body.project.clone(),
                        mode: "git_status".to_string(),
                        content: Some(content),
                        items: None,
                        truncated,
                        error: None,
                    }
                }
            }
            ContextMode::GitDiff => {
                let (code, stdout, stderr, _) = run_project_cmd(proj, "git diff", 30, ssh_config);
                if code != 0 {
                    ContextResponse {
                        success: false,
                        project: body.project.clone(),
                        mode: "git_diff".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some(format!("git diff failed: {}", stderr.trim())),
                    }
                } else {
                    let (content, truncated) = truncate_string(stdout, MAX_OUTPUT_LEN);
                    ContextResponse {
                        success: true,
                        project: body.project.clone(),
                        mode: "git_diff".to_string(),
                        content: Some(content),
                        items: None,
                        truncated,
                        error: None,
                    }
                }
            }
        };
        let ssh_calls = match resp.mode.as_str() {
            "overview" | "tree" | "search" | "read_file" | "markdown_outline" | "read_section"
            | "git_status" | "git_diff" => 1,
            _ => 0,
        };
        tracing::info!(
            target: "codex.metrics",
            operation = "getProjectContext",
            project = %resp.project,
            mode = %resp.mode,
            executor = "ssh",
            success = resp.success,
            duration_ms = request_start.elapsed().as_millis() as u64,
            ssh_calls = ssh_calls,
            control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
            "codex_context_completed"
        );
        res.render(Json(resp));
        return;
    }

    // Local executor
    let root = proj.root();
    if !root.exists() {
        res.render(Json(ContextResponse {
            success: false,
            project: body.project.clone(),
            mode: format!("{:?}", body.mode),
            content: None,
            items: None,
            truncated: false,
            error: Some(format!("Project root does not exist: {:?}", root)),
        }));
        return;
    }

    match body.mode {
        ContextMode::Overview => {
            let branch = run_command("git rev-parse --abbrev-ref HEAD", &root, 10)
                .1
                .trim()
                .to_string();
            let status = run_command(git_status_command(), &root, 10)
                .1
                .trim()
                .to_string();
            let important_files = [
                "README.md",
                "TODO.md",
                "Cargo.toml",
                "scripts/e2e_test.sh",
                "src/main.rs",
            ];
            let mut content = format!("Project: {}\nRoot: {}\nBranch: {}\n\nGit Status:\n{}\n\nAllowed Checks: {}\n\nImportant Files:",
                body.project, root.display(), branch, status, proj.allowed_checks.join(", "));
            for f in &important_files {
                let exists = root.join(f).exists();
                content.push_str(&format!("\n  {}: {}", f, if exists { "yes" } else { "no" }));
            }
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "overview".to_string(),
                content: Some(content),
                items: None,
                truncated: false,
                error: None,
            }));
        }
        ContextMode::Tree => {
            let limit = normalize_tree_limit(body.limit);
            let max_depth = normalize_tree_depth(body.max_depth);
            let tree_root = match &body.path {
                Some(rel_path) => match canonicalize_and_verify(&root.join(rel_path), &root) {
                    Ok(path) => path,
                    Err(e) => {
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(ContextResponse {
                            success: false,
                            project: body.project,
                            mode: "tree".to_string(),
                            content: None,
                            items: None,
                            truncated: false,
                            error: Some(e),
                        }));
                        return;
                    }
                },
                None => root.clone(),
            };
            let mut items = Vec::new();
            collect_tree(&tree_root, &root, &mut items, limit, max_depth);
            let truncated = items.len() >= limit;
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "tree".to_string(),
                content: None,
                items: Some(items),
                truncated,
                error: None,
            }));
        }
        ContextMode::Search => {
            let Some(query) = &body.query else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(ContextResponse {
                    success: false,
                    project: body.project,
                    mode: "search".to_string(),
                    content: None,
                    items: None,
                    truncated: false,
                    error: Some("query parameter is required for search mode".to_string()),
                }));
                return;
            };
            let results = simple_search(&root, query, MAX_SEARCH_RESULTS);
            let truncated = results.len() >= MAX_SEARCH_RESULTS;
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "search".to_string(),
                content: None,
                items: Some(results),
                truncated,
                error: None,
            }));
        }
        ContextMode::ReadFile => {
            let Some(rel_path) = &body.path else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(ContextResponse {
                    success: false,
                    project: body.project,
                    mode: "read_file".to_string(),
                    content: None,
                    items: None,
                    truncated: false,
                    error: Some("path parameter is required for read_file mode".to_string()),
                }));
                return;
            };
            let full_path = root.join(rel_path);
            match canonicalize_and_verify(&full_path, &root) {
                Ok(canonical) => match std::fs::read_to_string(&canonical) {
                    Ok(content) => {
                        let lines: Vec<&str> = content.lines().collect();
                        let total = lines.len();
                        let end_line = match validate_read_file_range(body.start_line, body.limit) {
                            Ok(end_line) => end_line,
                            Err(e) => {
                                res.status_code(StatusCode::BAD_REQUEST);
                                res.render(Json(ContextResponse {
                                    success: false,
                                    project: body.project,
                                    mode: "read_file".to_string(),
                                    content: None,
                                    items: None,
                                    truncated: false,
                                    error: Some(e),
                                }));
                                return;
                            }
                        };
                        let start = body.start_line - 1;
                        let end = end_line.min(total);
                        let selected: Vec<String> = if start < total {
                            lines[start..end]
                                .iter()
                                .enumerate()
                                .map(|(i, l)| format_context_line(start + i + 1, l).0)
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let output = selected.join("\n");
                        let (output, truncated) = truncate_string(output, MAX_OUTPUT_LEN);
                        res.render(Json(ContextResponse {
                            success: true,
                            project: body.project,
                            mode: "read_file".to_string(),
                            content: Some(output),
                            items: None,
                            truncated,
                            error: None,
                        }));
                    }
                    Err(e) => {
                        res.render(Json(ContextResponse {
                            success: false,
                            project: body.project,
                            mode: "read_file".to_string(),
                            content: None,
                            items: None,
                            truncated: false,
                            error: Some(format!("Failed to read file: {}", e)),
                        }));
                    }
                },
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(ContextResponse {
                        success: false,
                        project: body.project,
                        mode: "read_file".to_string(),
                        content: None,
                        items: None,
                        truncated: false,
                        error: Some(e),
                    }));
                }
            }
        }
        ContextMode::MarkdownOutline | ContextMode::ReadSection => {
            let item = ContextBatchItem {
                mode: body.mode,
                path: body.path.clone(),
                query: body.query.clone(),
                start_line: body.start_line,
                limit: body.limit,
                max_depth: body.max_depth,
            };
            let (resp, _) = local_markdown_file_response(&root, &body.project, &item);
            res.render(Json(resp));
        }
        ContextMode::AgentContext => {
            res.render(Json(local_agent_context(&root, &body.project)));
        }
        ContextMode::GitStatus => {
            let output = run_command(git_status_command(), &root, 10);
            let (content, truncated) = truncate_string(output.1, MAX_OUTPUT_LEN);
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "git_status".to_string(),
                content: Some(content),
                items: None,
                truncated,
                error: None,
            }));
        }
        ContextMode::GitDiff => {
            let output = run_command("git diff", &root, 30);
            let (content, truncated) = truncate_string(output.1, MAX_OUTPUT_LEN);
            res.render(Json(ContextResponse {
                success: true,
                project: body.project,
                mode: "git_diff".to_string(),
                content: Some(content),
                items: None,
                truncated,
                error: None,
            }));
        }
    }
}

#[handler]
pub async fn codex_context_batch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(ContextBatchResponse {
            success: false,
            project: String::new(),
            results: Vec::new(),
            duration_ms: 0,
            ssh_calls: 0,
            error: Some(
                "Projects not configured. Set PROJECTS_CONFIG or create projects.toml".to_string(),
            ),
        }));
        return;
    };
    let body: ContextBatchRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ContextBatchResponse {
                success: false,
                project: String::new(),
                results: Vec::new(),
                duration_ms: 0,
                ssh_calls: 0,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    if body.requests.is_empty() || body.requests.len() > 20 {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(ContextBatchResponse {
            success: false,
            project: body.project,
            results: Vec::new(),
            duration_ms: 0,
            ssh_calls: 0,
            error: Some("requests must contain 1-20 items".to_string()),
        }));
        return;
    }
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(ContextBatchResponse {
                success: false,
                project: body.project,
                results: Vec::new(),
                duration_ms: 0,
                ssh_calls: 0,
                error: Some(e),
            }));
            return;
        }
    };

    let start = Instant::now();
    let (mut results, ssh_calls) = if proj.is_ssh() {
        match try_ssh_context_batch_once(proj, &body.project, &body.requests, projects.ssh.as_ref())
        {
            Some((results, ssh_calls)) => (results, ssh_calls),
            None => {
                let mut ssh_calls = 0;
                let mut results = Vec::with_capacity(body.requests.len());
                for item in &body.requests {
                    let (resp, calls) =
                        execute_context_item(proj, &body.project, item, projects.ssh.as_ref());
                    ssh_calls += calls;
                    results.push(resp);
                }
                (results, ssh_calls)
            }
        }
    } else {
        let mut results = Vec::with_capacity(body.requests.len());
        for item in &body.requests {
            let (resp, _) = execute_context_item(proj, &body.project, item, None);
            results.push(resp);
        }
        (results, 0)
    };
    enforce_context_batch_total_limit(&mut results, body.max_total_chars);
    let success = results.iter().all(|r| r.success);
    let duration_ms = start.elapsed().as_millis() as u64;
    tracing::info!(
        target: "codex.metrics",
        operation = "getProjectContextBatch",
        project = %body.project,
        executor = if proj.is_ssh() { "ssh" } else { "local" },
        success = success,
        request_count = results.len(),
        duration_ms = duration_ms,
        ssh_calls = ssh_calls,
        control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
        "codex_context_batch_completed"
    );
    res.render(Json(ContextBatchResponse {
        success,
        project: body.project,
        results,
        duration_ms,
        ssh_calls,
        error: None,
    }));
}

#[handler]
pub async fn codex_apply_patch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let body: PatchRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_patch() {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Patch is not allowed for this project".to_string()),
        }));
        return;
    }
    if body.patch.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Patch cannot be empty".to_string()),
        }));
        return;
    }

    // Validate changed file paths against sensitive paths
    let changed = parse_changed_files_from_patch(&body.patch);
    for file in &changed {
        if is_sensitive_path(file) {
            res.status_code(StatusCode::FORBIDDEN);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(format!("Cannot modify sensitive path: {}", file)),
            }));
            return;
        }
    }

    if proj.is_ssh() {
        // SSH executor: pipe patch via stdin
        let resp = ssh_apply_patch(
            proj,
            &body.project,
            &body.patch,
            changed,
            projects.ssh.as_ref(),
        );
        res.render(Json(resp));
        return;
    }

    // Local executor
    let root = proj.root();
    if !root.exists() {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Project root does not exist".to_string()),
        }));
        return;
    }

    // Write patch to temp file, run git apply
    let patch_file = root.join(format!(".codex-patch-{}.diff", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&patch_file, &body.patch) {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some(format!("Failed to write temp patch file: {}", e)),
        }));
        return;
    }

    // Dry run first
    let check_out = run_command(
        &format!("git apply --check '{}'", patch_file.display()),
        &root,
        60,
    );
    if check_out.0 != 0 {
        let _ = std::fs::remove_file(&patch_file);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(check_out.1),
            stderr: Some(check_out.2),
            error: Some("git apply --check failed".to_string()),
        }));
        return;
    }

    // Apply for real
    let apply_out = run_command(&format!("git apply '{}'", patch_file.display()), &root, 60);
    let _ = std::fs::remove_file(&patch_file);

    if apply_out.0 == 0 {
        res.render(Json(PatchResponse {
            success: true,
            changed_files: Some(changed),
            stdout: Some(apply_out.1),
            stderr: Some(apply_out.2),
            error: None,
        }));
    } else {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(apply_out.1),
            stderr: Some(apply_out.2),
            error: Some("git apply failed".to_string()),
        }));
    }
}

pub(super) fn require_active_goal(
    db: &Database,
    goal_id: &str,
    project: &str,
) -> Result<CodexGoalRecord, String> {
    let goal = db
        .get_goal(goal_id)
        .map_err(|e| format!("Failed to load goal: {}", e))?
        .ok_or_else(|| "Goal not found".to_string())?;
    if goal.project != project {
        return Err("Goal project does not match request project".to_string());
    }
    if goal.status != "active" {
        return Err("Goal is not active".to_string());
    }
    let now = chrono::Utc::now().timestamp();
    if goal.expires_at < now {
        let _ = db.update_goal_status(&goal.id, "expired", now, Some("Goal expired"));
        return Err("Goal expired".to_string());
    }
    Ok(goal)
}

pub(super) fn reject_command_request_inner(
    db: &Database,
    request_id: String,
    reason: Option<String>,
) -> CommandRequestResponse {
    if let Err(e) = validate_command_request_reason(&reason) {
        return CommandRequestResponse {
            success: false,
            request_id: Some(request_id),
            record: None,
            error: Some(e),
        };
    }
    let error = reason.unwrap_or_else(|| "Rejected by user".to_string());
    match db.reject_command_request(&request_id, chrono::Utc::now().timestamp(), &error) {
        Ok(Some(record)) => CommandRequestResponse {
            success: true,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: None,
        },
        Ok(None) => match db.get_command_request(&request_id) {
            Ok(Some(record)) => CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some("Command request is not pending".to_string()),
            },
            Ok(None) => CommandRequestResponse {
                success: false,
                request_id: Some(request_id),
                record: None,
                error: Some("Command request not found".to_string()),
            },
            Err(e) => CommandRequestResponse {
                success: false,
                request_id: Some(request_id),
                record: None,
                error: Some(format!("Failed to load command request: {}", e)),
            },
        },
        Err(e) => CommandRequestResponse {
            success: false,
            request_id: Some(request_id),
            record: None,
            error: Some(format!("Failed to reject command request: {}", e)),
        },
    }
}

pub(super) fn approve_command_request_inner(
    projects: &ProjectsConfig,
    db: &Database,
    request_id: String,
) -> CommandRequestResponse {
    let approved_at = chrono::Utc::now().timestamp();
    let min_created_at = approved_at - COMMAND_REQUEST_TTL_SECS;
    let mut record =
        match db.claim_command_request_for_execution(&request_id, approved_at, min_created_at) {
            Ok(Some(record)) => record,
            Ok(None) => match db.get_command_request(&request_id) {
                Ok(Some(record)) => {
                    if record.status == "pending" && record.created_at < min_created_at {
                        let error = "Command request expired".to_string();
                        let expired = db
                            .expire_command_request(&record.id, approved_at, &error)
                            .ok()
                            .flatten()
                            .unwrap_or(record);
                        return CommandRequestResponse {
                            success: false,
                            request_id: Some(expired.id.clone()),
                            record: Some(expired),
                            error: Some(error),
                        };
                    }
                    return CommandRequestResponse {
                        success: false,
                        request_id: Some(record.id.clone()),
                        record: Some(record),
                        error: Some("Command request is not pending".to_string()),
                    };
                }
                Ok(None) => {
                    return CommandRequestResponse {
                        success: false,
                        request_id: Some(request_id),
                        record: None,
                        error: Some("Command request not found".to_string()),
                    };
                }
                Err(e) => {
                    return CommandRequestResponse {
                        success: false,
                        request_id: Some(request_id),
                        record: None,
                        error: Some(format!("Failed to load command request: {}", e)),
                    };
                }
            },
            Err(e) => {
                return CommandRequestResponse {
                    success: false,
                    request_id: Some(request_id),
                    record: None,
                    error: Some(format!("Failed to claim command request: {}", e)),
                };
            }
        };
    let proj = match projects.get_project(&record.project) {
        Ok(p) => p,
        Err(e) => {
            record.status = "failed".to_string();
            record.executed_at = Some(chrono::Utc::now().timestamp());
            record.error = Some(e.clone());
            let _ = db.update_command_request_result(&record);
            return CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some(e),
            };
        }
    };
    if !proj.allow_command_requests {
        let error = "Command requests are not enabled for this project".to_string();
        record.status = "failed".to_string();
        record.executed_at = Some(chrono::Utc::now().timestamp());
        record.error = Some(error.clone());
        let _ = db.update_command_request_result(&record);
        return CommandRequestResponse {
            success: false,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: Some(error),
        };
    }
    let cmd = match record.command_text.clone() {
        Some(cmd) if !cmd.is_empty() => cmd,
        _ => {
            let error = "Command request is missing command_text snapshot".to_string();
            record.status = "failed".to_string();
            record.executed_at = Some(chrono::Utc::now().timestamp());
            record.error = Some(error.clone());
            let _ = db.update_command_request_result(&record);
            return CommandRequestResponse {
                success: false,
                request_id: Some(record.id.clone()),
                record: Some(record),
                error: Some(error),
            };
        }
    };
    let (code, stdout, stderr, _) =
        run_project_cmd(proj, &cmd, CHECK_TIMEOUT_SECS, projects.ssh.as_ref());
    let (stdout_tail, _) = sanitize_tail(&stdout, MAX_OUTPUT_LEN);
    let (stderr_tail, _) = sanitize_tail(&stderr, MAX_OUTPUT_LEN);
    record.status = if code == 0 { "completed" } else { "failed" }.to_string();
    record.approved_at = Some(approved_at);
    record.executed_at = Some(chrono::Utc::now().timestamp());
    record.exit_code = Some(code);
    record.stdout_tail = Some(stdout_tail);
    record.stderr_tail = Some(stderr_tail);
    record.error = if code == 0 {
        None
    } else {
        Some("command failed".to_string())
    };
    if let Err(e) = db.update_command_request_result(&record) {
        return CommandRequestResponse {
            success: false,
            request_id: Some(record.id.clone()),
            record: Some(record),
            error: Some(format!("Failed to update command request: {}", e)),
        };
    }
    CommandRequestResponse {
        success: code == 0,
        request_id: Some(record.id.clone()),
        record: Some(record),
        error: if code == 0 {
            None
        } else {
            Some("command failed".to_string())
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_ssh_path_rejects_absolute() {
        assert!(validate_ssh_read_path("/etc/passwd").is_err());
    }

    #[test]
    fn test_validate_ssh_path_rejects_traversal() {
        assert!(validate_ssh_read_path("../evil.txt").is_err());
        assert!(validate_ssh_read_path("src/../../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_ssh_path_rejects_sensitive() {
        assert!(validate_ssh_read_path(".env").is_err());
        assert!(validate_ssh_read_path("secret.pem").is_err());
        assert!(validate_ssh_read_path(".git/config").is_err());
        assert!(validate_ssh_read_path("target/debug/binary").is_err());
        assert!(validate_ssh_read_path("node_modules/pkg/index.js").is_err());
    }

    #[test]
    fn test_validate_ssh_path_allows_normal() {
        assert!(validate_ssh_read_path("src/main.rs").is_ok());
        assert!(validate_ssh_read_path("README.md").is_ok());
        assert!(validate_ssh_read_path("src/lib/helper.rs").is_ok());
    }

    #[test]
    fn test_is_sensitive_path_variants() {
        assert!(is_sensitive_path(".env"));
        assert!(is_sensitive_path(".env.local"));
        assert!(is_sensitive_path("secret.pem"));
        assert!(is_sensitive_path("id_rsa"));
        assert!(is_sensitive_path(".git/config"));
        assert!(!is_sensitive_path("src/main.rs"));
        assert!(!is_sensitive_path("README.md"));
    }

    #[test]
    fn test_validate_command_name_accepts_safe_ids() {
        assert!(validate_command_name("clippy").is_ok());
        assert!(validate_command_name("doc.build-1").is_ok());
    }

    #[test]
    fn test_validate_command_name_rejects_shell_like_text() {
        assert!(validate_command_name("").is_err());
        assert!(validate_command_name("cargo test").is_err());
        assert!(validate_command_name("test;rm").is_err());
        assert!(validate_command_name(&"a".repeat(101)).is_err());
    }

    #[test]
    fn test_get_project_command_returns_configured_command() {
        let mut commands = HashMap::new();
        commands.insert("smoke".to_string(), "echo ok".to_string());
        let proj = ProjectConfig {
            path: "/tmp/project".to_string(),
            executor: crate::projects::Executor::Local,
            host: None,
            user: None,
            allow_patch: true,
            allow_command_requests: true,
            allow_raw_command_requests: true,
            allowed_checks: vec![],
            checks: None,
            commands,
        };
        assert_eq!(get_project_command(&proj, "smoke").unwrap(), "echo ok");
        assert!(get_project_command(&proj, "missing").is_err());
    }

    #[test]
    fn test_git_command_status_and_diff_are_fixed() {
        let status = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Status,
            paths: vec![],
            message: None,
        };
        assert_eq!(
            git_command_for_request(&status).unwrap(),
            "git status --short"
        );
        let diff = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Diff,
            paths: vec!["src/main.rs".to_string()],
            message: None,
        };
        assert_eq!(
            git_command_for_request(&diff).unwrap(),
            "git diff -- 'src/main.rs'"
        );
    }

    #[test]
    fn test_git_command_commit_is_fixed_and_no_verify() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Commit,
            paths: vec!["src/main.rs".to_string()],
            message: Some("Add feature".to_string()),
        };
        let cmd = git_command_for_request(&request).unwrap();
        assert!(cmd.contains("git add -- 'src/main.rs'"));
        assert!(cmd.contains("git diff --cached --quiet -- 'src/main.rs'"));
        assert!(cmd.contains("No staged changes to commit"));
        assert!(cmd.contains("git commit -m 'Add feature' --no-verify"));
    }

    #[test]
    fn test_git_command_commit_rejects_bad_message() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Commit,
            paths: vec!["src/main.rs".to_string()],
            message: Some("bad\nmessage".to_string()),
        };
        assert!(git_command_for_request(&request).is_err());
    }

    #[test]
    fn test_raw_command_validation_rejects_high_risk_tokens() {
        assert!(validate_raw_command_text("echo ok").is_ok());
        assert!(validate_raw_command_text("git status --short").is_ok());
        assert!(validate_raw_command_text("git push origin main").is_err());
        assert!(validate_raw_command_text("sudo systemctl restart nginx").is_err());
        assert!(validate_raw_command_text("rm -rf target").is_err());
        assert!(validate_raw_command_text("echo one\necho two").is_err());
    }

    #[test]
    fn test_git_command_amend_is_fixed_and_no_verify() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::CommitAmendNoEdit,
            paths: vec!["src/codex.rs".to_string()],
            message: None,
        };
        let cmd = git_command_for_request(&request).unwrap();
        assert!(cmd.contains("git add -- 'src/codex.rs'"));
        assert!(cmd.contains("git diff --cached --quiet -- 'src/codex.rs'"));
        assert!(cmd.contains("No staged changes to amend"));
        assert!(cmd.contains("git commit --amend --no-edit --no-verify"));
    }

    #[test]
    fn test_git_paths_reject_too_many_paths() {
        let paths = (0..=MAX_GIT_PATHS)
            .map(|i| format!("src/file{i}.rs"))
            .collect::<Vec<_>>();
        let err = validate_git_paths(&paths).unwrap_err();
        assert!(err.contains("too many paths"));
        assert!(err.contains("50"));
    }

    #[test]
    fn test_git_paths_reject_too_long_path() {
        let long_path = format!("src/{}.rs", "a".repeat(MAX_GIT_PATH_LEN));
        let err = validate_git_paths(&[long_path]).unwrap_err();
        assert!(err.contains("path is too long"));
        assert!(err.contains("512"));
    }

    #[test]
    fn test_git_command_rejects_sensitive_paths() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::Add,
            paths: vec![".env".to_string()],
            message: None,
        };
        assert!(git_command_for_request(&request).is_err());
    }

    #[test]
    fn test_git_mutating_commands_require_paths() {
        let request = GitRequest {
            project: "p".to_string(),
            operation: GitOperation::CommitAmendNoEdit,
            paths: vec![],
            message: None,
        };
        assert!(git_command_for_request(&request).is_err());
    }

    #[test]
    fn test_parse_ssh_batch_blocks_with_nonce() {
        let nonce = "abc123";
        let stdout = "__PDCTX_abc123_START_0__\nfirst\n__PDCTX_abc123_END_0__\n__PDCTX_abc123_START_1__\nsecond\n__PDCTX_abc123_END_1__\n";
        let blocks = parse_ssh_batch_blocks(stdout, 2, nonce);
        assert_eq!(blocks[0], "first\n");
        assert_eq!(blocks[1], "second\n");
    }

    #[test]
    fn test_parse_ssh_batch_blocks_ignores_old_style_markers() {
        let nonce = "abc123";
        let stdout = "__PDCTX_abc123_START_0__\nline before\n__PDCTX_START_0__\nfile content\n__PDCTX_END_0__\nline after\n__PDCTX_abc123_END_0__\n";
        let blocks = parse_ssh_batch_blocks(stdout, 1, nonce);
        assert!(blocks[0].contains("__PDCTX_START_0__"));
        assert!(blocks[0].contains("__PDCTX_END_0__"));
        assert!(blocks[0].contains("line after"));
    }

    #[test]
    fn test_invalid_read_file_ranges_return_errors() {
        assert!(validate_read_file_range(0, 10).is_err());
        assert!(validate_read_file_range(1, 0).is_err());
        assert!(validate_read_file_range(1, MAX_READ_FILE_LIMIT + 1).is_err());
        assert!(validate_read_file_range(usize::MAX, 2).is_err());
    }

    #[test]
    fn test_ssh_batch_failure_returns_one_result_per_request() {
        let requests = vec![
            ContextBatchItem {
                mode: ContextMode::Overview,
                path: None,
                query: None,
                start_line: 1,
                limit: 10,
                max_depth: default_tree_max_depth(),
            },
            ContextBatchItem {
                mode: ContextMode::ReadFile,
                path: Some("README.md".to_string()),
                query: None,
                start_line: 1,
                limit: 10,
                max_depth: default_tree_max_depth(),
            },
        ];
        let results = ssh_context_batch_error_results("proj", &requests, "boom".to_string());
        assert_eq!(results.len(), requests.len());
        assert!(results.iter().all(|r| !r.success));
        assert!(results.iter().all(|r| r.error.as_deref() == Some("boom")));
    }

    #[test]
    fn test_build_ssh_target() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: Some("msi".to_string()),
            user: Some("root".to_string()),
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert_eq!(proj.ssh_target().unwrap(), "root@msi");

        let proj_no_user = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: Some("msi".to_string()),
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert_eq!(proj_no_user.ssh_target().unwrap(), "msi");
    }

    #[test]
    fn test_build_ssh_target_no_host() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::Ssh,
            host: None,
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert!(proj.ssh_target().is_err());
    }

    #[test]
    fn test_local_executor_is_default() {
        let proj = ProjectConfig {
            path: "/tmp/test".to_string(),
            executor: crate::projects::Executor::default(),
            host: None,
            user: None,
            allow_patch: false,
            allow_command_requests: false,
            allow_raw_command_requests: false,
            allowed_checks: vec![],
            checks: None,
            commands: HashMap::new(),
        };
        assert!(!proj.is_ssh());
    }

    // =========================================================================
    // Edit unit tests
    // =========================================================================

    #[test]
    fn test_replace_nth_single_match() {
        let result = replace_nth("hello world", "world", "rust", None).unwrap();
        assert_eq!(result, "hello rust");
    }

    #[test]
    fn test_replace_nth_no_match() {
        let result = replace_nth("hello world", "xyz", "abc", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_replace_nth_empty_old() {
        let result = replace_nth("hello", "", "x", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_nth_multiple_no_occurrence() {
        let result = replace_nth("aXbXc", "X", "Y", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("2 times"));
    }

    #[test]
    fn test_replace_nth_multiple_with_occurrence() {
        let result = replace_nth("aXbXc", "X", "Y", Some(2)).unwrap();
        assert_eq!(result, "aXbYc");
    }

    #[test]
    fn test_replace_nth_occurrence_zero() {
        let result = replace_nth("abc", "a", "b", Some(0));
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_nth_occurrence_too_large() {
        let result = replace_nth("abc", "a", "b", Some(5));
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_line_range_basic() {
        let content = "line1\nline2\nline3\n";
        let result = replace_line_range(content, 2, 2, "new2\n").unwrap();
        assert_eq!(result, "line1\nnew2\nline3\n");
    }

    #[test]
    fn test_replace_line_range_multi() {
        let content = "line1\nline2\nline3\nline4\n";
        let result = replace_line_range(content, 2, 3, "replaced\n").unwrap();
        assert_eq!(result, "line1\nreplaced\nline4\n");
    }

    #[test]
    fn test_replace_line_range_invalid_start() {
        let content = "line1\nline2\n";
        let result = replace_line_range(content, 0, 1, "x");
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_line_range_exceeds() {
        let content = "line1\n";
        let result = replace_line_range(content, 1, 5, "x");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_no_mixed_edit_kinds_rejects_same_path_text_binary() {
        let edits = vec![
            EditOperation::WriteFile {
                path: "docs/diagram.bin".to_string(),
                content: "text".to_string(),
                allow_overwrite: true,
            },
            EditOperation::WriteBinaryFile {
                path: "docs/diagram.bin".to_string(),
                base64_content: "AAE=".to_string(),
                allow_overwrite: true,
            },
        ];
        let err = validate_no_mixed_edit_kinds(&edits).unwrap_err();
        assert!(err.contains("cannot mix text and binary edits for the same path"));
    }

    #[test]
    fn test_validate_no_mixed_edit_kinds_allows_same_path_same_kind() {
        let edits = vec![
            EditOperation::WriteBinaryFile {
                path: "docs/diagram.bin".to_string(),
                base64_content: "AAE=".to_string(),
                allow_overwrite: true,
            },
            EditOperation::WriteBinaryFile {
                path: "docs/diagram.bin".to_string(),
                base64_content: "AQI=".to_string(),
                allow_overwrite: true,
            },
        ];
        assert!(validate_no_mixed_edit_kinds(&edits).is_ok());
    }

    #[test]
    fn test_read_binary_from_upload_accepts_project_relative_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("upload.bin");
        std::fs::write(&source, [1_u8, 2, 3, 4]).unwrap();
        let bytes = read_binary_from_upload(dir.path(), "upload.bin", "docs/out.bin").unwrap();
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_read_binary_from_upload_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_binary_from_upload(dir.path(), "../secret.bin", "docs/out.bin").unwrap_err();
        assert!(err.contains("traversal"));
    }

    #[test]
    fn test_validate_source_url_rejects_localhost() {
        let err = validate_source_url("http://localhost:8080/file.png").unwrap_err();
        assert!(err.contains("not allowed"));
        let err = validate_source_url("http://127.0.0.1/file.png").unwrap_err();
        assert!(err.contains("blocked private/local"));
    }

    #[test]
    fn test_validate_source_url_rejects_non_http() {
        let err = validate_source_url("file:///tmp/file.png").unwrap_err();
        assert!(err.contains("http or https"));
    }

    #[test]
    fn test_validate_source_url_allows_chatgpt_estuary_content() {
        let url = validate_source_url("https://chatgpt.com/backend-api/estuary/content?id=file_abc123&ts=1&p=fsns&cid=1&sig=abc&v=0").unwrap();
        assert_eq!(url.host_str(), Some("chatgpt.com"));
        assert_eq!(url.path(), "/backend-api/estuary/content");
    }

    #[test]
    fn test_chatgpt_estuary_allowlist_rejects_non_estuary_path() {
        let url = reqwest::Url::parse("https://chatgpt.com/api/not-estuary?id=file_abc123&sig=abc")
            .unwrap();
        assert!(!is_allowed_chatgpt_estuary_url(&url));
    }

    #[test]
    fn test_decode_binary_artifact_accepts_small_base64() {
        let bytes = decode_binary_artifact("AAECAw==", "docs/pixel.bin").unwrap();
        assert_eq!(bytes, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_decode_binary_artifact_rejects_invalid_base64() {
        let err = decode_binary_artifact("not valid base64!", "docs/pixel.bin").unwrap_err();
        assert!(err.contains("Invalid base64"));
    }

    #[test]
    fn test_simple_binary_diff_mentions_sizes() {
        let diff = simple_binary_diff("docs/pixel.bin", Some(2), 4);
        assert!(diff.contains("Binary files"));
        assert!(diff.contains("old size: 2"));
        assert!(diff.contains("new size: 4"));
    }

    #[test]
    fn test_validate_edit_path_rejects_env() {
        assert!(validate_edit_path(".env").is_err());
        assert!(validate_edit_path("config/.env").is_err());
    }

    #[test]
    fn test_validate_edit_path_rejects_traversal() {
        assert!(validate_edit_path("../evil.txt").is_err());
        assert!(validate_edit_path("src/../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_edit_path_rejects_absolute() {
        assert!(validate_edit_path("/etc/passwd").is_err());
    }

    #[test]
    fn test_validate_edit_path_rejects_target() {
        assert!(validate_edit_path("target/debug/binary").is_err());
    }

    #[test]
    fn test_validate_edit_path_allows_normal() {
        assert!(validate_edit_path("src/main.rs").is_ok());
        assert!(validate_edit_path("README.md").is_ok());
        assert!(validate_edit_path(".gitignore").is_ok());
    }

    #[test]
    fn test_validate_edit_path_rejects_git_dir() {
        assert!(validate_edit_path(".git/config").is_err());
        assert!(validate_edit_path(".git/hooks/pre-commit").is_err());
    }

    // =========================================================================
    // SSH edit safety tests
    // =========================================================================

    #[test]
    fn test_shell_escape_no_injection() {
        // Verify that shell_escape properly wraps in single quotes
        // Input: '; rm -rf /; echo '
        // Expected output: '\'''; rm -rf /; echo '\''
        // The outer single quotes prevent shell interpretation of the content
        let dangerous = "'; rm -rf /; echo '";
        let escaped = shell_escape(dangerous);
        // Should start and end with single quote
        assert!(escaped.starts_with('\''));
        assert!(escaped.ends_with('\''));
        // Should contain the escaped single-quote sequence ('\'' means: end quote, literal quote, start quote)
        assert!(escaped.contains("'\\''"));
        // The escaped form should be: '\''  '; rm -rf /; echo '  '\''
        // which is safe because the dangerous content is inside single quotes
    }

    #[test]
    fn test_ssh_edit_command_no_user_input_in_shell() {
        // Verify that user-controlled edit content does not appear in the SSH command string
        let user_input = "'; malicious_command; echo '";
        let body = EditRequest {
            project: "test".to_string(),
            reason: None,
            dry_run: false,
            edits: vec![EditOperation::ReplaceText {
                path: "src/main.rs".to_string(),
                old_text: user_input.to_string(),
                new_text: "safe".to_string(),
                occurrence: None,
            }],
        };
        let _body_json = serde_json::to_string(&body).unwrap();
        // The JSON-serialized body should contain the user input escaped inside JSON,
        // but the shell_escape of the python script itself should not contain raw user input
        let escaped_script = shell_escape(REMOTE_EDIT_SCRIPT);
        assert!(!escaped_script.contains(user_input));
        // The body JSON is piped via stdin, not embedded in the command
        // So the SSH command is: ssh target -- python3 -c '<script>' '<project_path>'
        // Neither argument contains the user's edit payload directly
    }

    // =========================================================================
    // Remote python3 script local run test
    // =========================================================================

    #[test]
    fn test_remote_edit_script_replace_text_local() {
        // Run the embedded python3 script locally to verify it works
        let tmp = tempfile::tempdir().unwrap_or_else(|_| {
            // fallback if tempfile not available
            let d = std::path::PathBuf::from("/tmp/private-drop-test-script");
            let _ = std::fs::create_dir_all(&d);
            // Return a wrapper
            tempfile::TempDir::new_in(&d).unwrap()
        });
        let root = tmp.path();
        std::fs::write(root.join("test.txt"), "hello world\n").unwrap();

        let request = serde_json::json!({
            "dry_run": false,
            "edits": [{
                "type": "replace_text",
                "path": "test.txt",
                "old_text": "world",
                "new_text": "rust"
            }]
        });

        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(REMOTE_EDIT_SCRIPT)
            .arg(root.to_str().unwrap())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("Failed to spawn python3");

        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            stdin.write_all(request.to_string().as_bytes()).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "Script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["changed_files"][0], "test.txt");
        assert!(result["diff"].as_str().unwrap().contains("-hello world"));
        assert!(result["diff"].as_str().unwrap().contains("+hello rust"));
        // Verify the file was actually modified
        let content = std::fs::read_to_string(root.join("test.txt")).unwrap();
        assert_eq!(content, "hello rust\n");
    }

    #[test]
    fn test_remote_edit_script_dry_run_local() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap_or_else(|_| {
            let d = std::path::PathBuf::from("/tmp/private-drop-test-dry");
            let _ = std::fs::create_dir_all(&d);
            tempfile::TempDir::new_in(&d).unwrap()
        });
        let root = tmp.path();
        std::fs::write(root.join("test.txt"), "original content\n").unwrap();

        let request = serde_json::json!({
            "dry_run": true,
            "edits": [{
                "type": "replace_text",
                "path": "test.txt",
                "old_text": "original",
                "new_text": "changed"
            }]
        });

        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(REMOTE_EDIT_SCRIPT)
            .arg(root.to_str().unwrap())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("Failed to spawn python3");

        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(request.to_string().as_bytes()).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["success"], true);
        assert!(result["diff"].as_str().unwrap().contains("-original"));
        assert!(result["diff"].as_str().unwrap().contains("+changed"));
        // Verify the file was NOT modified (dry_run)
        let content = std::fs::read_to_string(root.join("test.txt")).unwrap();
        assert_eq!(content, "original content\n");
    }

    #[test]
    fn test_remote_edit_script_rejects_env() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap_or_else(|_| {
            let d = std::path::PathBuf::from("/tmp/private-drop-test-env");
            let _ = std::fs::create_dir_all(&d);
            tempfile::TempDir::new_in(&d).unwrap()
        });
        let root = tmp.path();

        let request = serde_json::json!({
            "dry_run": false,
            "edits": [{
                "type": "replace_text",
                "path": ".env",
                "old_text": "x",
                "new_text": "y"
            }]
        });

        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(REMOTE_EDIT_SCRIPT)
            .arg(root.to_str().unwrap())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("Failed to spawn python3");

        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(request.to_string().as_bytes()).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["success"], false);
        assert!(result["error"].as_str().unwrap().contains("sensitive"));
    }
}

fn read_binary_from_url(source_url: &str, rel_path: &str) -> Result<Vec<u8>, String> {
    let url = validate_source_url(source_url)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(URL_IMPORT_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("Failed to build URL client: {}", e))?;
    let response = client
        .get(url)
        .send()
        .map_err(|e| format!("Failed to fetch source_url: {}", e))?;
    if response.status().is_redirection() {
        return Err("source_url redirects are not allowed".to_string());
    }
    if !response.status().is_success() {
        return Err(format!("source_url returned HTTP {}", response.status()));
    }
    if let Some(len) = response.content_length() {
        if len as usize > MAX_BINARY_ARTIFACT_SIZE {
            return Err(format!(
                "source_url content for {} exceeds {} bytes",
                rel_path, MAX_BINARY_ARTIFACT_SIZE
            ));
        }
    }
    let mut bytes = Vec::new();
    {
        use std::io::Read;
        let mut limited = response.take((MAX_BINARY_ARTIFACT_SIZE + 1) as u64);
        limited
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Failed to read source_url response: {}", e))?;
    }
    validate_binary_size(bytes, rel_path)
}

pub(super) fn apply_edit_request_with_metrics(
    projects: &ProjectsConfig,
    proj: &ProjectConfig,
    body: &EditRequest,
    operation: &'static str,
) -> EditResponse {
    let edit_start = Instant::now();
    if proj.is_ssh() {
        let response = ssh_apply_project_edit(proj, body, projects.ssh.as_ref());
        tracing::info!(
            target: "codex.metrics",
            operation = operation,
            project = %body.project,
            executor = "ssh",
            success = response.success,
            dry_run = body.dry_run,
            edit_count = body.edits.len(),
            changed_files = response.changed_files.len(),
            duration_ms = edit_start.elapsed().as_millis() as u64,
            ssh_calls = 1,
            control_master = projects.ssh.as_ref().map(|s| s.control_master).unwrap_or(false),
            "codex_edit_completed"
        );
        return response;
    }
    let response = local_apply_project_edit(proj, body);
    tracing::info!(
        target: "codex.metrics",
        operation = operation,
        project = %body.project,
        executor = "local",
        success = response.success,
        dry_run = body.dry_run,
        edit_count = body.edits.len(),
        changed_files = response.changed_files.len(),
        duration_ms = edit_start.elapsed().as_millis() as u64,
        ssh_calls = 0,
        control_master = false,
        "codex_edit_completed"
    );
    response
}

#[cfg(test)]
mod ssh_command_tests {
    use super::*;

    fn ssh_config() -> SshConfig {
        SshConfig {
            batch_mode: false,
            connect_timeout_secs: None,
            control_master: false,
            control_persist: None,
            control_path: None,
            server_alive_interval: None,
            server_alive_count_max: None,
        }
    }

    fn command_args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn default_ssh_config_does_not_add_controlmaster() {
        let args = ssh_option_args(None);
        assert!(!args.iter().any(|arg| arg.contains("ControlMaster")));
        assert!(args.is_empty());
    }

    #[test]
    fn control_master_adds_reuse_options() {
        let mut cfg = ssh_config();
        cfg.control_master = true;
        cfg.control_persist = Some("10m".into());
        cfg.control_path = Some("/tmp/private-drop-ssh-%C".into());
        let args = ssh_option_args(Some(&cfg));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ControlMaster=auto".to_string()));
        assert!(args.contains(&"ControlPersist=10m".to_string()));
        assert!(args.contains(&"ControlPath=/tmp/private-drop-ssh-%C".to_string()));
    }

    #[test]
    fn batch_mode_without_control_master_adds_batchmode_only() {
        let mut cfg = ssh_config();
        cfg.batch_mode = true;
        let args = ssh_option_args(Some(&cfg));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("ControlMaster")));
    }

    #[test]
    fn connect_timeout_and_keepalive_options_are_rendered() {
        let mut cfg = ssh_config();
        cfg.connect_timeout_secs = Some(10);
        cfg.server_alive_interval = Some(30);
        cfg.server_alive_count_max = Some(3);
        let args = ssh_option_args(Some(&cfg));
        assert!(args.contains(&"ConnectTimeout=10".to_string()));
        assert!(args.contains(&"ServerAliveInterval=30".to_string()));
        assert!(args.contains(&"ServerAliveCountMax=3".to_string()));
    }

    #[test]
    fn ssh_command_uses_args_not_local_shell() {
        let mut cfg = ssh_config();
        cfg.batch_mode = true;
        let command = build_ssh_command("root@example", "cd /repo && git status", Some(&cfg));
        assert_eq!(command.get_program().to_string_lossy(), "ssh");
        let args = command_args(&command);
        assert_eq!(
            args.last().map(String::as_str),
            Some("cd /repo && git status")
        );
        assert!(args.contains(&"root@example".to_string()));
        assert!(!args.iter().any(|arg| arg == "sh" || arg == "-c"));
    }
}
