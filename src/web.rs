use salvo::prelude::*;

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
<link rel="stylesheet" href="/assets/styles.css">
<script src="/assets/app.js" defer></script>
</head>
<body>
<div class="container">
<div class="header"><h1>Private Drop</h1></div>
<div class="nav"><a href="/channels">Channels</a><a href="/c/inbox">Inbox</a><a href="/c/files">Files</a><a href="/send">Send</a><a href="/desktop">Desktop</a><a href="/agent/playground">Agent</a></div>
<div id="app"><div class="loading">Loading...</div></div>
</div>
<script defer>
window.addEventListener('DOMContentLoaded', function() {{
{page_js}
}});
</script>
</body>
</html>"#,
        title = html_escape(title),
        page_js = page_js
    )
}

#[handler]
pub async fn frontend_app_js(res: &mut Response) {
    res.add_header(
        "content-type",
        "application/javascript; charset=utf-8",
        true,
    )
    .ok();
    res.render(Text::Plain(include_str!("../frontend/dist/app.js")));
}

#[handler]
pub async fn frontend_styles_css(res: &mut Response) {
    res.add_header("content-type", "text/css; charset=utf-8", true)
        .ok();
    res.render(Text::Plain(include_str!("../frontend/dist/styles.css")));
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
pub async fn desktop_page(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let page_js = r#"
(async function(){
    if(!requireToken())return;
    var app=document.getElementById('app');
    function taskCard(t){
        var shot=t.screenshot_url?'<div style="margin-top:8px"><a href="'+escapeHtml(t.screenshot_url)+'" target="_blank">Screenshot</a></div>':'';
        return '<a href="/desktop/tasks/'+encodeURIComponent(t.id)+'" style="color:inherit;text-decoration:none"><div class="card">'+
            '<div class="card-header"><div><div class="card-title">'+escapeHtml(t.title)+'</div><div class="card-meta">'+fmtTime(t.updated_at)+' · '+escapeHtml(t.claimed_by||'')+'</div></div><span class="channel-badge">'+escapeHtml(t.status)+'</span></div>'+
            '<div class="card-text">'+escapeHtml(t.last_event||t.instructions||'')+'</div>'+shot+'</div></a>';
    }
    async function loadTasks(){
        var r=await apiCall('/api/desktop/tasks?limit=20');
        if(!r)return;
        if(!r.ok)return;
        var d=await r.json();
        var html='';
        (d.tasks||[]).forEach(function(t){html+=taskCard(t)});
        document.getElementById('task-list').innerHTML=html||'<div class="card"><p>No desktop tasks yet</p></div>';
    }
    app.innerHTML='<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px"><h2>Desktop Agent</h2><button id="refresh" class="btn btn-sm btn-primary">Refresh</button></div>'+ 
        '<div id="msg"></div><div class="card"><h3 style="margin-bottom:12px">Send Desktop Task</h3>'+ 
        '<form id="df">'+
        '<div class="form-group"><label for="title">Title</label><input id="title" placeholder="Open site and type message" required></div>'+ 
        '<div class="form-group"><label for="url">URL to open (optional)</label><input id="url" placeholder="https://example.com"></div>'+ 
        '<div class="form-group"><label for="text">Text to type/send (optional)</label><textarea id="text" rows="5" placeholder="Text the worker should paste into the active page/app"></textarea></div>'+ 
        '<div class="form-group"><label><input type="checkbox" id="sendKey"> Press Enter after typing</label></div>'+ 
        '<div class="form-group"><label for="extra">Extra instructions</label><textarea id="extra" rows="4" placeholder="Wait for page load, click the input first if needed..."></textarea></div>'+ 
        '<div class="form-actions"><button class="btn btn-primary" type="submit">Create Task</button></div></form></div>'+ 
        '<h3 style="margin:20px 0 12px">Recent Desktop Tasks</h3><div id="task-list"><div class="loading">Loading...</div></div>';
    document.getElementById('refresh').addEventListener('click',loadTasks);
    document.getElementById('df').addEventListener('submit',async function(e){
        e.preventDefault();
        var title=document.getElementById('title').value.trim();
        var url=document.getElementById('url').value.trim();
        var text=document.getElementById('text').value;
        var extra=document.getElementById('extra').value.trim();
        var sendKey=document.getElementById('sendKey').checked;
        var parts=[];
        if(url)parts.push('open: '+url);
        if(text)parts.push('type: '+text);
        if(sendKey)parts.push('press_enter: true');
        if(extra)parts.push(extra);
        var instructions=parts.join('\n');
        if(!instructions){instructions=title}
        var r=await apiCall('/api/desktop/tasks',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({title:title,instructions:instructions,priority:10})});
        if(!r)return;
        var d=await r.json();
        if(r.ok&&d.task){window.location.href='/desktop/tasks/'+encodeURIComponent(d.task.id)}else{document.getElementById('msg').innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to create task')+'</div>'}
    });
    await loadTasks();
})()
"#;
    res.render(Text::Html(app_shell("Desktop", page_js)));
}

#[handler]
pub async fn desktop_task_page(req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let id = req.param::<String>("id").unwrap_or_default();
    let page_js = format!(
        r#"
(async function(){{
    if(!requireToken())return;
    var taskId={id_json};
    var app=document.getElementById('app');
    function row(label,value){{return '<div style="margin:6px 0"><strong>'+escapeHtml(label)+':</strong> '+escapeHtml(value||'')+'</div>'}}
    try{{
        var r=await apiCall('/api/desktop/tasks/'+encodeURIComponent(taskId));
        if(!r)return;
        if(!r.ok){{app.innerHTML='<div class="alert alert-error">Desktop task not found</div>';return}}
        var d=await r.json();
        var t=d.task;
        var events=d.events||[];
        var html='<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px">'+
            '<div><a href="/channels" style="display:inline-block;margin-bottom:8px;color:#3498db;text-decoration:none">← Back</a>'+
            '<h2>Desktop Task</h2></div><span class="channel-badge">'+escapeHtml(t.status)+'</span></div>';
        html+='<div class="card"><div class="card-title">'+escapeHtml(t.title)+'</div>'+
            row('ID',t.id)+row('Worker',t.claimed_by||'')+row('Priority',String(t.priority))+row('Updated',fmtTime(t.updated_at))+
            '<div style="margin-top:12px"><strong>Instructions</strong><div class="card-text" style="max-height:none">'+escapeHtml(t.instructions)+'</div></div>';
        if(t.screenshot_url){{html+='<div style="margin-top:16px"><a href="'+escapeHtml(t.screenshot_url)+'" target="_blank"><img src="'+escapeHtml(t.screenshot_url)+'" style="max-width:100%;border:1px solid #ddd;border-radius:8px"></a></div>'}}
        html+='</div>';
        html+='<h3 style="margin:20px 0 12px">Event Timeline</h3>';
        if(events.length===0){{html+='<div class="card"><p>No events recorded yet</p></div>'}}else{{
            events.forEach(function(ev){{
                html+='<div class="card"><div class="card-header"><div><div class="card-title">'+escapeHtml(ev.status)+'</div><div class="card-meta">'+fmtTime(ev.created_at)+' '+escapeHtml(ev.worker||'')+'</div></div></div>'+
                    '<div class="card-text" style="max-height:none">'+escapeHtml(ev.message||'')+'</div>';
                if(ev.screenshot_url){{html+='<div style="margin-top:12px"><a href="'+escapeHtml(ev.screenshot_url)+'" target="_blank">Open screenshot</a></div>'}}
                html+='</div>';
            }});
        }}
        app.innerHTML=html;
    }}catch(e){{
        app.innerHTML='<div class="alert alert-error">Error: '+escapeHtml(e.message)+'</div>';
    }}
}})()
"#,
        id_json = serde_json::to_string(&id).unwrap()
    );
    res.render(Text::Html(app_shell("Desktop Task", &page_js)));
}

#[handler]
pub async fn agent_playground_page(_req: &mut Request, _depot: &mut Depot, res: &mut Response) {
    let page_js = r#"
(async function(){
    if(!requireToken())return;
    var app=document.getElementById('app');
    var specs=[];
    var selectedSpec=null;
    function pretty(v){try{return JSON.stringify(v,null,2)}catch(e){return String(v)}}
    function toolList(tools){
        if(!tools||tools.length===0)return '<div class="card"><p>No tools parsed yet</p></div>';
        return tools.map(function(t){
            return '<div class="card" style="margin-bottom:8px">'+
                '<div class="card-title">'+escapeHtml(t.name)+'</div>'+
                '<div class="card-meta">'+escapeHtml(t.method+' '+t.path)+'</div>'+
                '<div class="card-text">'+escapeHtml(t.description||'')+'</div>'+
            '</div>';
        }).join('');
    }
    function renderTimeline(events){
        if(!events||events.length===0)return '<div class="card"><p>No run yet</p></div>';
        return events.map(function(ev){
            if(ev.type==='assistant_message'){
                return '<div class="card"><div class="card-header"><div><div class="card-title">Assistant</div><div class="card-meta">round '+ev.round+' · '+ev.latency_ms+'ms</div></div></div>'+
                    '<div class="card-text" style="max-height:none;white-space:pre-wrap">'+escapeHtml(ev.content||'')+'</div></div>';
            }
            if(ev.type==='tool_call'){
                return '<div class="card"><div class="card-header"><div><div class="card-title">Tool call: '+escapeHtml(ev.name)+'</div><div class="card-meta">round '+ev.round+' · '+escapeHtml(ev.tool_call_id)+'</div></div></div>'+
                    '<pre class="card-text" style="max-height:none;white-space:pre-wrap">'+escapeHtml(pretty(ev.arguments))+'</pre></div>';
            }
            if(ev.type==='tool_response'){
                return '<div class="card"><div class="card-header"><div><div class="card-title">Tool response: '+escapeHtml(ev.name)+'</div><div class="card-meta">status '+escapeHtml(ev.status||'n/a')+' · '+ev.duration_ms+'ms</div></div></div>'+
                    (ev.truncated?'<div class="card-meta" style="margin-bottom:8px">response truncated</div>':'')+
                    (ev.error?'<div class="alert alert-error">'+escapeHtml(ev.error)+'</div>':'')+
                    '<pre class="card-text" style="max-height:none;white-space:pre-wrap">'+escapeHtml(ev.response_preview||'')+'</pre></div>';
            }
            return '<div class="alert alert-error">'+escapeHtml(ev.message||'Unknown error')+'</div>';
        }).join('');
    }
    function renderShell(){
        app.innerHTML=
            '<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px"><h2>Tool Calling Playground</h2><button id="reload" class="btn btn-sm btn-primary">Reload</button></div>'+
            '<div class="alert alert-info">MVP only supports POST application/json tools. Prefer importing codex-openapi-compact.json.</div>'+
            '<div id="msg"></div>'+
            '<div class="card"><h3 style="margin-bottom:12px">Model profile</h3>'+
                '<div class="form-group"><label for="modelBase">base_url</label><input id="modelBase" placeholder="https://api.openai.com/v1"></div>'+
                '<div class="form-group"><label for="modelKey">api_key</label><input id="modelKey" type="password" placeholder="Stored on server; leave blank to reuse saved key"></div>'+
                '<div class="form-group"><label for="modelName">model</label><input id="modelName" placeholder="gpt-4.1-mini"></div>'+
                '<div style="display:grid;grid-template-columns:1fr 1fr;gap:12px">'+
                    '<div class="form-group"><label for="temp">temperature</label><input id="temp" type="number" min="0" max="2" step="0.1" value="0.2"></div>'+
                    '<div class="form-group"><label for="rounds">max_rounds</label><input id="rounds" type="number" min="1" max="20" value="6"></div>'+
                '</div>'+
            '</div>'+
            '<div class="card"><h3 style="margin-bottom:12px">Action spec</h3>'+
                '<div class="form-group"><label for="specSelect">Saved spec</label><select id="specSelect"><option value="">Select spec</option></select></div>'+
                '<div id="selectedMeta" class="card-meta" style="margin-bottom:12px"></div>'+
                '<div style="display:grid;grid-template-columns:1fr 1fr;gap:12px">'+
                    '<div class="form-group"><label for="specName">name</label><input id="specName" placeholder="private-drop compact"></div>'+
                    '<div class="form-group"><label for="actionBase">base_url</label><input id="actionBase" placeholder="https://drop.example.com"></div>'+
                '</div>'+
                '<div class="form-group"><label for="actionToken">bearer token</label><input id="actionToken" type="password" placeholder="Stored on server after save"></div>'+
                '<div class="form-group"><label for="openapiJson">OpenAPI JSON</label><textarea id="openapiJson" rows="12" spellcheck="false" placeholder="{...}"></textarea></div>'+
                '<div class="form-actions"><button id="saveSpec" class="btn btn-primary">Save spec</button><button id="deleteSpec" class="btn btn-danger" type="button">Delete selected</button></div>'+
            '</div>'+
            '<h3 style="margin:20px 0 12px">Parsed tools</h3><div id="tools"></div>'+
            '<div class="card"><h3 style="margin-bottom:12px">Chat</h3>'+
                '<div class="form-group"><label for="systemPrompt">system prompt</label><textarea id="systemPrompt" rows="5">You are a tool-calling debugging agent. Use the available actions when needed, then explain the result briefly.</textarea></div>'+
                '<div class="form-group"><label for="userMessage">user message</label><textarea id="userMessage" rows="5" placeholder="Ask the model to inspect or run something..."></textarea></div>'+
                '<div class="form-actions"><button id="sendRun" class="btn btn-success">Send</button></div>'+
            '</div>'+
            '<h3 style="margin:20px 0 12px">Timeline</h3><div id="timeline"><div class="card"><p>No run yet</p></div></div>';
        document.getElementById('reload').addEventListener('click',loadSpecs);
        document.getElementById('saveSpec').addEventListener('click',saveSpec);
        document.getElementById('deleteSpec').addEventListener('click',deleteSpec);
        document.getElementById('sendRun').addEventListener('click',runAgent);
        document.getElementById('specSelect').addEventListener('change',selectSpec);
    }
    function fillProfile(profile){
        profile=profile||{};
        document.getElementById('modelBase').value=profile.base_url||'';
        document.getElementById('modelName').value=profile.model||'';
        document.getElementById('temp').value=profile.temperature==null?'0.2':String(profile.temperature);
        document.getElementById('rounds').value=profile.max_rounds||6;
        var key=document.getElementById('modelKey');
        key.value='';
        key.placeholder=profile.api_key_masked?('Saved: '+profile.api_key_masked):'Stored on server; leave blank to reuse saved key';
    }
    function selectSpec(){
        var id=document.getElementById('specSelect').value;
        selectedSpec=specs.find(function(s){return s.id===id})||null;
        if(!selectedSpec){
            document.getElementById('selectedMeta').textContent='';
            document.getElementById('tools').innerHTML='';
            return;
        }
        document.getElementById('specName').value=selectedSpec.name;
        document.getElementById('actionBase').value=selectedSpec.base_url;
        document.getElementById('actionToken').value='';
        document.getElementById('actionToken').placeholder=selectedSpec.auth_token_masked?('Saved: '+selectedSpec.auth_token_masked):'No token saved';
        document.getElementById('selectedMeta').textContent='id '+selectedSpec.id+' · token '+(selectedSpec.auth_token_masked||'empty');
        document.getElementById('tools').innerHTML=toolList(selectedSpec.tools);
        loadSpecDetail(selectedSpec.id);
    }
    async function loadSpecDetail(id){
        var r=await apiCall('/api/agent/specs/'+encodeURIComponent(id));
        if(!r||!r.ok)return;
        var d=await r.json();
        if(document.getElementById('specSelect').value===id){
            selectedSpec=d;
            document.getElementById('openapiJson').value=d.openapi_json||'';
            document.getElementById('tools').innerHTML=toolList(d.tools);
        }
    }
    async function loadSpecs(){
        var msg=document.getElementById('msg');
        msg.innerHTML='';
        var r=await apiCall('/api/agent/specs');
        if(!r)return;
        var d=await r.json();
        if(!r.ok){msg.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to load specs')+'</div>';return}
        specs=d.specs||[];
        fillProfile(d.model_profile);
        var sel=document.getElementById('specSelect');
        sel.innerHTML='<option value="">Select spec</option>'+specs.map(function(s){return '<option value="'+escapeHtml(s.id)+'">'+escapeHtml(s.name)+'</option>'}).join('');
        if(specs[0]){sel.value=specs[0].id;selectSpec()}else{document.getElementById('tools').innerHTML='<div class="card"><p>No saved specs yet</p></div>'}
    }
    async function saveSpec(){
        var msg=document.getElementById('msg');
        msg.innerHTML='';
        var payload={
            id:selectedSpec?selectedSpec.id:null,
            name:document.getElementById('specName').value.trim(),
            base_url:document.getElementById('actionBase').value.trim(),
            auth_token:document.getElementById('actionToken').value,
            openapi_json:document.getElementById('openapiJson').value
        };
        var r=await apiCall('/api/agent/specs',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(payload)});
        if(!r)return;
        var d=await r.json();
        if(!r.ok){msg.innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Failed to save spec')+'</div>';return}
        msg.innerHTML='<div class="alert alert-success">Spec saved</div>';
        await loadSpecs();
        document.getElementById('specSelect').value=d.id;
        selectSpec();
    }
    async function deleteSpec(){
        if(!selectedSpec)return;
        if(!confirm('Delete selected spec?'))return;
        var r=await apiCall('/api/agent/specs/'+encodeURIComponent(selectedSpec.id),{method:'DELETE'});
        if(!r)return;
        await loadSpecs();
    }
    async function runAgent(){
        var msg=document.getElementById('msg');
        msg.innerHTML='';
        if(!selectedSpec){msg.innerHTML='<div class="alert alert-error">Select a saved spec first</div>';return}
        document.getElementById('timeline').innerHTML='<div class="loading">Running...</div>';
        var payload={
            spec_id:selectedSpec.id,
            model_base_url:document.getElementById('modelBase').value.trim(),
            model_api_key:document.getElementById('modelKey').value,
            model:document.getElementById('modelName').value.trim(),
            temperature:Number(document.getElementById('temp').value||0.2),
            max_rounds:Number(document.getElementById('rounds').value||6),
            system_prompt:document.getElementById('systemPrompt').value,
            user_message:document.getElementById('userMessage').value
        };
        var r=await apiCall('/api/agent/run',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(payload)});
        if(!r)return;
        var d=await r.json();
        if(!r.ok){document.getElementById('timeline').innerHTML='<div class="alert alert-error">'+escapeHtml(d.error||'Run failed')+'</div>';return}
        var final=d.final_response?'<div class="card"><div class="card-title">Final</div><div class="card-text" style="max-height:none;white-space:pre-wrap">'+escapeHtml(d.final_response)+'</div><div class="card-meta">'+escapeHtml(d.stopped_reason||'')+'</div></div>':'';
        document.getElementById('timeline').innerHTML=final+renderTimeline(d.timeline||[]);
        document.getElementById('modelKey').value='';
        await loadSpecs();
    }
    renderShell();
    await loadSpecs();
})()
"#;
    res.render(Text::Html(app_shell("Agent Playground", page_js)));
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
