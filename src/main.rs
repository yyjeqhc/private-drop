#![recursion_limit = "256"]

use salvo::cors::Cors;
use salvo::prelude::*;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
#[cfg(test)]
use uuid::Uuid;

mod auth;
mod codex;
mod config;
mod db;
mod drop_api;
mod models;
mod openapi;
mod projects;

pub(crate) use auth::{get_config, get_db, json_error, AuthMiddleware};
pub(crate) use config::load_startup_env_files;
#[cfg(test)]
pub(crate) use config::parse_env_file_line;
pub use config::Config;
pub use db::Database;
pub(crate) use drop_api::{
    create_message, delete_message, download_file, get_message, health, list_channels,
    list_messages, upload_file,
};
pub use models::{
    Channel, CodexGoalRecord, CommandAuditRecord, CreateMessageRequest, Message, MessageKind,
};
pub(crate) use openapi::{codex_openapi_compact_json, codex_openapi_json, openapi_json};

// ============================================================================
// Web UI
// ============================================================================

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn app_shell(title: &str, page_js: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8"><meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{title} - Private Drop</title>
<style>
*{{margin:0;padding:0;box-sizing:border-box}}
body{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:#f5f5f5;color:#333}}
.container{{max-width:800px;margin:0 auto;padding:16px}}
.header{{background:#2c3e50;color:white;padding:16px;margin-bottom:16px;border-radius:8px}}
.header h1{{font-size:1.5em}}
.nav{{display:flex;gap:8px;margin-bottom:16px;flex-wrap:wrap}}
.nav a{{padding:8px 16px;background:#3498db;color:white;text-decoration:none;border-radius:4px;font-size:14px}}
.nav a:hover{{background:#2980b9}}
.card{{background:white;border-radius:8px;padding:16px;margin-bottom:12px;box-shadow:0 2px 4px rgba(0,0,0,0.1)}}
.card-header{{display:flex;justify-content:space-between;align-items:center;margin-bottom:8px}}
.card-title{{font-weight:bold;font-size:1.1em;word-break:break-word}}
.card-meta{{color:#666;font-size:0.9em}}
.card-text{{white-space:pre-wrap;word-break:break-word;line-height:1.5}}
.btn{{padding:8px 16px;border:none;border-radius:4px;cursor:pointer;font-size:14px;text-decoration:none;display:inline-block}}
.btn-primary{{background:#3498db;color:white}}
.btn-danger{{background:#e74c3c;color:white}}
.btn-success{{background:#27ae60;color:white}}
.btn-sm{{padding:4px 12px;font-size:12px}}
.form-group{{margin-bottom:12px}}
.form-group label{{display:block;margin-bottom:4px;font-weight:bold}}
.form-group input,.form-group textarea,.form-group select{{width:100%;padding:10px;border:1px solid #ddd;border-radius:4px;font-size:14px}}
.form-group textarea{{min-height:150px;resize:vertical}}
.form-actions{{display:flex;gap:8px}}
.file-info{{display:flex;align-items:center;gap:8px}}
.file-icon{{font-size:24px}}
.file-size{{color:#666;font-size:0.9em}}
.token-form{{max-width:400px;margin:100px auto}}
.alert{{padding:12px;border-radius:4px;margin-bottom:12px}}
.alert-error{{background:#f8d7da;color:#721c24}}
.alert-success{{background:#d4edda;color:#155724}}
.channel-badge{{display:inline-block;padding:2px 8px;background:#ecf0f1;border-radius:12px;font-size:0.8em}}
.loading{{text-align:center;padding:40px;color:#666}}
@media(max-width:600px){{.container{{padding:8px}}.header{{padding:12px}}.card{{padding:12px}}}}
</style>
</head>
<body>
<div class="container">
<div class="header"><h1>Private Drop</h1></div>
<div class="nav"><a href="/channels">Channels</a><a href="/c/inbox">Inbox</a><a href="/c/files">Files</a><a href="/send">Send</a></div>
<div id="app"><div class="loading">Loading...</div></div>
</div>
<script>
function getToken(){{return localStorage.getItem('drop_token')||''}}
function setToken(t){{localStorage.setItem('drop_token',t)}}
function clearToken(){{localStorage.removeItem('drop_token')}}
function requireToken(){{if(!getToken()){{window.location.href='/login';return false}}return true}}
async function apiCall(url,options={{}}){{const token=getToken();if(!token){{window.location.href='/login';return null}}const headers={{...options.headers}};headers['Authorization']='Bearer '+token;const resp=await fetch(url,{{...options,headers}});if(resp.status===401){{clearToken();window.location.href='/login';return null}}return resp}}
function escapeHtml(s){{const d=document.createElement('div');d.textContent=s;return d.innerHTML}}
function formatSize(b){{if(b<1024)return b+' B';if(b<1048576)return(b/1024).toFixed(1)+' KB';if(b<1073741824)return(b/1048576).toFixed(1)+' MB';return(b/1073741824).toFixed(2)+' GB'}}
function fmtTime(ts){{const d=new Date(ts*1000);return d.toLocaleString()}}
async function deleteMsg(id){{if(!confirm('Delete this message?'))return;const r=await apiCall('/api/messages/'+id,{{method:'DELETE'}});if(r&&r.ok)window.location.reload()}}
function copyText(t){{navigator.clipboard.writeText(t).then(()=>alert('Copied!'))}}
{page_js}
</script>
</body>
</html>"#,
        title = html_escape(title),
        page_js = page_js
    )
}

#[handler]
pub async fn login_page(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let page_js = r#"
(function(){
    if(getToken()){window.location.href='/c/inbox';return}
    document.getElementById('app').innerHTML=
        '<div class="token-form"><div class="card">'+
        '<h2 style="margin-bottom:16px">Login</h2>'+
        '<div id="err"></div>'+
        '<form id="lf">'+
        '<div class="form-group"><label for="token">Access Token</label>'+
        '<input type="password" id="token" placeholder="Enter your token" required autofocus></div>'+
        '<div class="form-actions"><button type="submit" class="btn btn-primary">Login</button></div>'+
        '</form></div></div>';
    document.getElementById('lf').addEventListener('submit',function(e){
        e.preventDefault();
        var t=document.getElementById('token').value.trim();
        if(!t)return;
        setToken(t);
        window.location.href='/c/inbox';
    });
})()
"#;
    res.render(Text::Html(app_shell("Login", page_js)));
}

#[handler]
pub async fn home_page(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    // Client-side: redirect to /c/inbox if logged in, else /login
    let page_js = r#"
(function(){
    if(!getToken()){window.location.href='/login';return}
    window.location.href='/channels';
})()
"#;
    res.render(Text::Html(app_shell("Home", page_js)));
}

#[handler]
pub async fn channels_page(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let page_js = r#"
(async function(){
    if(!requireToken())return;
    var app=document.getElementById('app');
    try{
        var r=await apiCall('/api/channels');
        if(!r)return;
        if(!r.ok){var d=await r.json();app.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to load channels')+'</div>';return}
        var channels=await r.json();
        var html='<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px"><h2>Channels</h2><a href="/send" class="btn btn-primary">Send</a></div>';
        if(channels.length===0){
            html+='<div class="card"><p style="color:#666;text-align:center">No channels yet</p></div>';
        }else{
            channels.forEach(function(ch){
                html+='<a href="/c/'+encodeURIComponent(ch.name)+'" style="color:inherit;text-decoration:none"><div class="card"><div class="card-header"><div><div class="card-title">'+escapeHtml(ch.display_name||ch.name)+'</div><div class="card-meta">'+escapeHtml(ch.name)+'</div></div><span class="channel-badge">'+ch.message_count+' message'+(ch.message_count===1?'':'s')+'</span></div></div></a>';
            });
        }
        app.innerHTML=html;
    }catch(e){
        app.innerHTML='<div class="alert alert-error">Error: '+escapeHtml(e.message)+'</div>';
    }
})()
"#;
    res.render(Text::Html(app_shell("Channels", page_js)));
}

#[handler]
pub async fn channel_page(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let channel = req.param::<String>("channel").unwrap_or_default();
    let page_js = format!(
        r#"
(async function(){{
    if(!requireToken())return;
    var ch={channel_json};
    var app=document.getElementById('app');
    try{{
        var r=await apiCall('/api/messages?channel='+encodeURIComponent(ch)+'&limit=50');
        if(!r)return;
        if(!r.ok){{var d=await r.json();app.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to load')+'</div>';return}}
        var data=await r.json();
        var html='<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px">'+
            '<div><a href="/channels" style="display:inline-block;margin-bottom:8px;color:#3498db;text-decoration:none">← Back to Channels</a>'+
            '<h2>'+escapeHtml(ch)+'</h2></div>'+
            '<a href="/send?channel='+encodeURIComponent(ch)+'" class="btn btn-primary">Send</a></div>';
        if(data.messages.length===0){{
            html+='<div class="card"><p style="color:#666;text-align:center">No messages yet</p></div>';
        }}else{{
            data.messages.forEach(function(m){{
                var title=m.title||(m.kind==='file'?(m.file_name||'File'):'Text');
                var ts=fmtTime(m.created_at);
                var body='';
                if(m.kind==='text'){{
                    var t=m.text||'';
                    body='<div class="card-text">'+escapeHtml(t.length>200?t.substring(0,200)+'...':t)+'</div>';
                }}else{{
                    body='<div class="file-info"><span class="file-icon">📎</span><div>'+
                        '<div style="font-weight:bold">'+escapeHtml(m.file_name||'unknown')+'</div>'+
                        '<div class="file-size">'+formatSize(m.file_size||0)+'</div></div></div>';
                }}
                var actions='';
                if(m.kind==='text'){{
                    actions='<button class="btn btn-sm btn-primary js-copy" data-text-id="t-'+m.id+'">Copy</button> '+
                        '<button class="btn btn-sm btn-danger js-delete" data-delete-id="'+m.id+'">Del</button>';
                }}else{{
                    actions='<a href="/api/files/'+m.id+'" class="btn btn-sm btn-success" download>Download</a> '+
                        '<button class="btn btn-sm btn-danger js-delete" data-delete-id="'+m.id+'">Del</button>';
                }}
                html+='<div class="card" id="t-'+m.id+'"><div class="card-header"><div>'+
                    '<div class="card-title"><a href="/m/'+m.id+'" style="color:inherit;text-decoration:none">'+escapeHtml(title)+'</a></div>'+
                    '<div class="card-meta">'+ts+'</div></div>'+
                    '<div class="form-actions">'+actions+'</div></div>'+body+'</div>';
            }});
        }}
        app.innerHTML=html;
        app.addEventListener('click',function(e){{
            var btn=e.target.closest('.js-copy');
            if(btn){{var el=document.getElementById(btn.getAttribute('data-text-id'));if(el)copyText(el.textContent);return}}
            btn=e.target.closest('.js-delete');
            if(btn){{deleteMsg(btn.getAttribute('data-delete-id'));return}}
        }});
    }}catch(e){{
        app.innerHTML='<div class="alert alert-error">Error: '+escapeHtml(e.message)+'</div>';
    }}
}})()
"#,
        channel_json = serde_json::to_string(&channel).unwrap()
    );
    res.render(Text::Html(app_shell(&channel, &page_js)));
}

#[handler]
pub async fn message_page(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let id = req.param::<String>("id").unwrap_or_default();
    let page_js = format!(
        r#"
(async function(){{
    if(!requireToken())return;
    var msgId={id_json};
    var app=document.getElementById('app');
    try{{
        var r=await apiCall('/api/messages/'+msgId);
        if(!r)return;
        if(!r.ok){{app.innerHTML='<div class="alert alert-error">Message not found</div>';return}}
        var m=await r.json();
        var ts=fmtTime(m.created_at);
        var html='<div class="card"><div style="display:flex;justify-content:space-between;margin-bottom:12px">'+
            '<div><span class="channel-badge">'+escapeHtml(m.channel)+'</span> <span class="card-meta">'+ts+'</span></div>'+
            '<div class="form-actions">';
        if(m.kind==='text'){{
            html+='<button class="btn btn-sm btn-primary js-copy" data-text-id="ft">Copy</button> ';
        }}else{{
            html+='<a href="/api/files/'+m.id+'" class="btn btn-sm btn-success" download>Download</a> ';
        }}
        html+='<button class="btn btn-sm btn-danger js-delete" data-delete-id="'+m.id+'">Del</button></div></div>';
        if(m.kind==='text'){{
            html+='<div id="ft" class="card-text" style="max-height:none">'+escapeHtml(m.text||'')+'</div>';
        }}else{{
            html+='<div class="file-info" style="font-size:1.2em"><span class="file-icon" style="font-size:48px">📎</span>'+
                '<div><div style="font-weight:bold;font-size:1.2em">'+escapeHtml(m.file_name||'unknown')+'</div>'+
                '<div class="file-size">'+formatSize(m.file_size||0)+'</div>'+
                '<div class="file-size">'+escapeHtml(m.mime_type||'')+'</div></div></div>';
        }}
        html+='</div>';
        var title=m.title||(m.kind==='file'?(m.file_name||'File'):'Message');
        app.innerHTML='<h2 style="margin-bottom:16px">'+escapeHtml(title)+'</h2>'+html;
        app.addEventListener('click',function(e){{
            var btn=e.target.closest('.js-copy');
            if(btn){{var el=document.getElementById(btn.getAttribute('data-text-id'));if(el)copyText(el.textContent);return}}
            btn=e.target.closest('.js-delete');
            if(btn){{deleteMsg(btn.getAttribute('data-delete-id'));return}}
        }});
    }}catch(e){{
        app.innerHTML='<div class="alert alert-error">Error: '+escapeHtml(e.message)+'</div>';
    }}
}})()
"#,
        id_json = serde_json::to_string(&id).unwrap()
    );
    res.render(Text::Html(app_shell("Message", &page_js)));
}

#[handler]
pub async fn send_page(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let default_channel = req
        .query::<String>("channel")
        .unwrap_or_else(|| "inbox".to_string());
    let page_js = format!(
        r#"
(async function(){{
    if(!requireToken())return;
    var defCh={channel_json};
    var app=document.getElementById('app');
    app.innerHTML=
        '<h2 style="margin-bottom:16px">Send Message</h2>'+
        '<div id="msg"></div>'+
        '<div class="card"><h3 style="margin-bottom:12px">Text Message</h3>'+
        '<form id="sf">'+
        '<div class="form-group"><label for="channel">Channel</label>'+
        '<select id="channel">'+
        '<option value="inbox">inbox</option>'+
        '<option value="xline">xline</option>'+
        '<option value="thesis">thesis</option>'+
        '<option value="packfix">packfix</option>'+
        '<option value="omo">omo</option>'+
        '<option value="files">files</option>'+
        '</select></div>'+
        '<div class="form-group"><label for="title">Title (optional)</label>'+
        '<input type="text" id="title" placeholder="Message title"></div>'+
        '<div class="form-group"><label for="text">Text</label>'+
        '<textarea id="text" placeholder="Paste your text here..." rows="10" required></textarea></div>'+
        '<div class="form-actions"><button type="submit" class="btn btn-primary">Send</button></div>'+
        '</form></div>'+
        '<div class="card" style="margin-top:16px"><h3 style="margin-bottom:12px">Upload File</h3>'+
        '<form id="ff">'+
        '<div class="form-group"><label for="file">File</label>'+
        '<input type="file" id="file" required></div>'+
        '<div class="form-actions"><button type="submit" class="btn btn-success">Upload</button></div>'+
        '</form></div>';
    document.getElementById('channel').value=defCh;
    document.getElementById('sf').addEventListener('submit',async function(e){{
        e.preventDefault();
        var ch=document.getElementById('channel').value;
        var title=document.getElementById('title').value||null;
        var text=document.getElementById('text').value;
        var msgEl=document.getElementById('msg');
        try{{
            var r=await apiCall('/api/messages',{{
                method:'POST',
                headers:{{'Content-Type':'application/json'}},
                body:JSON.stringify({{channel:ch,title:title,text:text}})
            }});
            if(!r)return;
            if(r.ok){{
                window.location.href='/c/'+encodeURIComponent(ch);
            }}else{{
                var d=await r.json();
                msgEl.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to send')+'</div>';
            }}
        }}catch(err){{
            msgEl.innerHTML='<div class="alert alert-error">'+escapeHtml(err.message)+'</div>';
        }}
    }});
    document.getElementById('ff').addEventListener('submit',async function(e){{
        e.preventDefault();
        var ch=document.getElementById('channel').value;
        var fileInput=document.getElementById('file');
        var msgEl=document.getElementById('msg');
        if(!fileInput.files[0])return;
        var fd=new FormData();
        fd.append('file',fileInput.files[0]);
        try{{
            var r=await apiCall('/api/files?channel='+encodeURIComponent(ch),{{method:'POST',body:fd}});
            if(!r)return;
            if(r.ok){{
                window.location.href='/c/'+encodeURIComponent(ch);
            }}else{{
                var d=await r.json();
                msgEl.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to upload')+'</div>';
            }}
        }}catch(err){{
            msgEl.innerHTML='<div class="alert alert-error">'+escapeHtml(err.message)+'</div>';
        }}
    }});
}})()
"#,
        channel_json = serde_json::to_string(&default_channel).unwrap()
    );
    res.render(Text::Html(app_shell("Send", &page_js)));
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env_loads = load_startup_env_files().map_err(std::io::Error::other)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    for load in &env_loads {
        tracing::info!(
            "Loaded env file {} ({} variables set)",
            load.path.display(),
            load.loaded_count
        );
    }
    let config = Config::from_env();
    if !config.is_auth_enabled() {
        tracing::warn!(
            "DROP_TOKEN is not set! Running in development mode without authentication."
        );
        tracing::warn!("Set DROP_TOKEN environment variable to enable authentication.");
    }
    tracing::info!("Starting Private Drop v{}", env!("CARGO_PKG_VERSION"));
    tracing::info!("Data directory: {:?}", config.data_dir);
    let addr = config.addr.clone();
    tracing::info!("Listening on: {}", addr);
    std::fs::create_dir_all(config.uploads_dir())?;
    let db = Database::open(&config.db_path())?;
    tracing::info!("Database initialized at {:?}", config.db_path());

    // Set max payload size to 2MB for text messages
    salvo::http::request::set_global_secure_max_size(config.max_text_size);

    // Load projects config for Codex API
    let projects_config = match projects::ProjectsConfig::load() {
        Ok(cfg) => {
            tracing::info!(
                "Loaded projects config with {} projects",
                cfg.projects.len()
            );
            Some(Arc::new(cfg))
        }
        Err(e) => {
            tracing::warn!(
                "Projects config not loaded: {}. Codex API will be disabled.",
                e
            );
            None
        }
    };

    let cors = Cors::permissive();
    let config = Arc::new(config);
    let db = Arc::new(db);

    let api_router = Router::with_path("api")
        .push(Router::with_path("health").get(health))
        .push(
            Router::new()
                .hoop(AuthMiddleware)
                .push(Router::with_path("channels").get(list_channels))
                .push(
                    Router::with_path("messages")
                        .get(list_messages)
                        .post(create_message),
                )
                .push(
                    Router::with_path("messages/{id}")
                        .get(get_message)
                        .delete(delete_message),
                )
                .push(Router::with_path("files/{file_id}").get(download_file))
                .push(Router::with_path("files").post(upload_file)),
        );

    let web_router = Router::new()
        .push(Router::with_path("login").get(login_page))
        .push(Router::with_path("channels").get(channels_page))
        .push(Router::with_path("send").get(send_page))
        .push(Router::with_path("c/{channel}").get(channel_page))
        .push(Router::with_path("m/{id}").get(message_page))
        .push(Router::with_path("").get(home_page));

    let openapi_router = Router::with_path("openapi.json").get(openapi_json);
    let codex_openapi_router = Router::with_path("codex-openapi.json").get(codex_openapi_json);
    let codex_openapi_compact_router =
        Router::with_path("codex-openapi-compact.json").get(codex_openapi_compact_json);

    let mut router = Router::new()
        .hoop(affix_state::inject(config.clone()))
        .hoop(affix_state::inject(db.clone()))
        .hoop(cors.into_handler())
        .push(api_router)
        .push(openapi_router)
        .push(codex_openapi_router)
        .push(codex_openapi_compact_router)
        .push(web_router);

    // Add Codex API routes if projects config is loaded
    if let Some(projects_cfg) = projects_config {
        router = router.hoop(affix_state::inject(projects_cfg)).push(
            Router::with_path("api/codex")
                .hoop(AuthMiddleware)
                .push(Router::with_path("context").post(codex::codex_context))
                .push(Router::with_path("context_batch").post(codex::codex_context_batch))
                .push(Router::with_path("apply_patch").post(codex::codex_apply_patch))
                .push(Router::with_path("edit").post(codex::codex_edit))
                .push(Router::with_path("artifact").post(codex::codex_artifact))
                .push(Router::with_path("git").post(codex::codex_git))
                .push(Router::with_path("command").post(codex::codex_command))
                .push(Router::with_path("command_request").post(codex::codex_command_request))
                .push(Router::with_path("command_request_op").post(codex::codex_command_request_op))
                .push(Router::with_path("job").post(codex::codex_job))
                .push(
                    Router::with_path("command_request_raw").post(codex::codex_command_request_raw),
                )
                .push(Router::with_path("command_requests").post(codex::codex_command_requests))
                .push(
                    Router::with_path("command_request_batch")
                        .post(codex::codex_command_request_batch),
                )
                .push(Router::with_path("command_approve").post(codex::codex_command_approve))
                .push(Router::with_path("command_reject").post(codex::codex_command_reject))
                .push(Router::with_path("check").post(codex::codex_check))
                .push(Router::with_path("report").post(codex::codex_report)),
        );
    }

    let acceptor = TcpListener::new(addr.clone()).bind().await;
    tracing::info!("Server started successfully!");
    let port = addr.split(':').last().unwrap_or("8080");
    tracing::info!("Web UI: http://localhost:{}", port);
    tracing::info!("API: http://localhost:{}/api", port);
    tracing::info!("OpenAPI: http://localhost:{}/openapi.json", port);
    tracing::info!(
        "Codex OpenAPI: http://localhost:{}/codex-openapi.json",
        port
    );
    tracing::info!(
        "Compact Codex OpenAPI: http://localhost:{}/codex-openapi-compact.json",
        port
    );
    Server::new(acceptor).serve(router).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_env_file_line_basic() {
        let parsed = parse_env_file_line("DROP_ADDR=127.0.0.1:8080")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.0, "DROP_ADDR");
        assert_eq!(parsed.1, "127.0.0.1:8080");
    }

    #[test]
    fn test_parse_env_file_line_quotes_and_export() {
        let parsed = parse_env_file_line("export RUST_LOG='info,codex.metrics=info'")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.0, "RUST_LOG");
        assert_eq!(parsed.1, "info,codex.metrics=info");
    }

    #[test]
    fn test_parse_env_file_line_ignores_empty_and_comments() {
        assert!(parse_env_file_line("").is_none());
        assert!(parse_env_file_line("  # comment").is_none());
    }

    #[test]
    fn test_parse_env_file_line_rejects_invalid_key() {
        assert!(parse_env_file_line("drop_token=x").unwrap().is_err());
        assert!(parse_env_file_line("DROP TOKEN=x").unwrap().is_err());
    }

    #[test]
    fn test_uuid_generation_not_empty() {
        let id = Uuid::new_v4().to_string();
        assert!(!id.is_empty());
        assert_eq!(id.len(), 36); // UUID v4 with hyphens
        assert!(id.contains('-'));
    }

    #[test]
    fn test_uuid_generation_unique() {
        let id1 = Uuid::new_v4().to_string();
        let id2 = Uuid::new_v4().to_string();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_config_from_env_defaults() {
        // Clear env vars to test defaults
        std::env::remove_var("DROP_ADDR");
        std::env::remove_var("DROP_DATA");
        std::env::remove_var("DROP_TOKEN");

        let config = Config::from_env();
        assert_eq!(config.addr, "0.0.0.0:8080");
        assert_eq!(config.data_dir, PathBuf::from("./data"));
        assert_eq!(config.token, None);
        assert!(!config.is_auth_enabled());
        assert_eq!(config.max_text_size, 2 * 1024 * 1024);
        assert_eq!(config.max_file_size, 100 * 1024 * 1024);
    }

    #[test]
    fn test_config_validate_token() {
        let config = Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: PathBuf::from("./data"),
            token: Some("secret123".to_string()),
            max_text_size: 2 * 1024 * 1024,
            max_file_size: 100 * 1024 * 1024,
        };
        assert!(config.is_auth_enabled());
        assert!(config.validate_token("secret123"));
        assert!(!config.validate_token("wrong"));
        assert!(!config.validate_token(""));
    }

    #[test]
    fn test_config_validate_token_none() {
        let config = Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: PathBuf::from("./data"),
            token: None,
            max_text_size: 2 * 1024 * 1024,
            max_file_size: 100 * 1024 * 1024,
        };
        assert!(!config.is_auth_enabled());
        // When no token is set, validation always returns false
        assert!(!config.validate_token("anything"));
    }

    #[test]
    fn test_filename_sanitization() {
        // Test that path separators are stripped from display names
        let filename = "test/file\\name.txt";
        let safe: String = filename
            .chars()
            .filter(|c| !matches!(c, '/' | '\\' | '\0' | '\r' | '\n'))
            .collect();
        assert_eq!(safe, "testfilename.txt");
    }

    #[test]
    fn test_filename_sanitization_quotes() {
        let filename = "file\"name.txt";
        let safe = filename.replace('"', "_");
        assert_eq!(safe, "file_name.txt");
    }

    #[test]
    fn test_command_request_claim_is_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("drop.db")).unwrap();
        let record = CommandAuditRecord {
            id: "req-1".to_string(),
            project: "p".to_string(),
            command: "smoke".to_string(),
            command_text: Some("echo ok".to_string()),
            reason: Some("test".to_string()),
            status: "pending".to_string(),
            created_at: 1,
            approved_at: None,
            executed_at: None,
            exit_code: None,
            stdout_tail: None,
            stderr_tail: None,
            error: None,
        };
        db.insert_command_request(&record).unwrap();
        let claimed = db
            .claim_command_request_for_execution("req-1", 2, 0)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, "running");
        assert_eq!(claimed.approved_at, Some(2));
        assert_eq!(claimed.command_text.as_deref(), Some("echo ok"));
        let second = db
            .claim_command_request_for_execution("req-1", 3, 0)
            .unwrap();
        assert!(second.is_none());
        let current = db.get_command_request("req-1").unwrap().unwrap();
        assert_eq!(current.status, "running");
        assert_eq!(current.approved_at, Some(2));
    }

    #[test]
    fn test_command_request_claim_respects_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("drop.db")).unwrap();
        let record = CommandAuditRecord {
            id: "old-req".to_string(),
            project: "p".to_string(),
            command: "smoke".to_string(),
            command_text: Some("echo ok".to_string()),
            reason: None,
            status: "pending".to_string(),
            created_at: 10,
            approved_at: None,
            executed_at: None,
            exit_code: None,
            stdout_tail: None,
            stderr_tail: None,
            error: None,
        };
        db.insert_command_request(&record).unwrap();
        let claimed = db
            .claim_command_request_for_execution("old-req", 100, 50)
            .unwrap();
        assert!(claimed.is_none());
        let current = db.get_command_request("old-req").unwrap().unwrap();
        assert_eq!(current.status, "pending");
    }

    #[test]
    fn test_command_request_reject_only_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("drop.db")).unwrap();
        let record = CommandAuditRecord {
            id: "reject-req".to_string(),
            project: "p".to_string(),
            command: "smoke".to_string(),
            command_text: Some("echo ok".to_string()),
            reason: None,
            status: "pending".to_string(),
            created_at: 1,
            approved_at: None,
            executed_at: None,
            exit_code: None,
            stdout_tail: None,
            stderr_tail: None,
            error: None,
        };
        db.insert_command_request(&record).unwrap();
        let rejected = db
            .reject_command_request("reject-req", 2, "no")
            .unwrap()
            .unwrap();
        assert_eq!(rejected.status, "rejected");
        assert_eq!(rejected.error.as_deref(), Some("no"));
        let second = db.reject_command_request("reject-req", 3, "again").unwrap();
        assert!(second.is_none());
    }

    #[test]
    fn test_message_serialization() {
        let msg = Message {
            id: "test-id".to_string(),
            channel: "inbox".to_string(),
            kind: MessageKind::Text,
            title: Some("Test".to_string()),
            text: Some("Hello".to_string()),
            file_name: None,
            file_path: None,
            file_size: None,
            mime_type: None,
            created_at: 1234567890,
            expires_at: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("test-id"));
        assert!(json.contains("inbox"));
        assert!(json.contains("text"));
    }
}
