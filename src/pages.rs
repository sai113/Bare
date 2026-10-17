//! Built-in pages: bare://downloads and bare://settings.
//!
//! Pages are generated here and loaded as data: URLs, which have an opaque origin no website can
//! script. Buttons "call" the browser by navigating to `https://bare.invalid/<secret>/<action>?..`;
//! `handlers` intercepts that navigation (it never hits the network) and only accepts it when the
//! per-session secret matches and the navigating frame is one of these pages.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    sync::LazyLock,
};

use cef::*;

use crate::{block::js_string, bookmarks, downloads, history, network, passwords, settings, system, update};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Downloads,
    Settings,
    History,
    Bookmarks,
    Passwords,
}

impl Page {
    pub fn name(self) -> &'static str {
        match self {
            Page::Downloads => "downloads",
            Page::Settings => "settings",
            Page::History => "history",
            Page::Bookmarks => "bookmarks",
            Page::Passwords => "passwords",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Page::Downloads => "Downloads",
            Page::Settings => "Settings",
            Page::History => "History",
            Page::Bookmarks => "Bookmarks",
            Page::Passwords => "Passwords",
        }
    }

    pub fn display_url(self) -> String {
        format!("bare://{}", self.name())
    }

    /// Parses what the user typed in the address bar ("bare://settings").
    pub fn from_address(input: &str) -> Option<Page> {
        let rest = input.trim().to_ascii_lowercase();
        let rest = rest.strip_prefix("bare://")?.trim_end_matches('/');
        [Page::Downloads, Page::Settings, Page::History, Page::Bookmarks, Page::Passwords].into_iter().find(|p| p.name() == rest)
    }
}

/// Random per-session secret.
static SECRET: LazyLock<String> = LazyLock::new(random_token);

/// 128 bits as hex that no page can guess: SipHash keyed from OS randomness (every call differs).
pub fn random_token() -> String {
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(std::process::id() as u64);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

const ACTION_HOST: &str = "https://bare.invalid/";

fn action_prefix() -> String {
    format!("{ACTION_HOST}{}/", *SECRET)
}

/// Is this a navigation to the action endpoint (with or without a valid secret)?
pub fn is_action_url(url: &str) -> bool {
    url.starts_with(ACTION_HOST)
}

/// Returns (action, params) if `url` carries this session's secret.
pub fn parse_action(url: &str) -> Option<(String, Vec<(String, String)>)> {
    let rest = url.strip_prefix(&action_prefix())?;
    let action = rest.split(['?', '#']).next()?.to_string();
    Some((action, crate::url::query(url)))
}

/// True for the data: URLs these pages are loaded from.
pub fn is_page_url(url: &str) -> bool {
    url.starts_with("data:text/html")
}

pub fn data_url(page: Page) -> String {
    let html = html(page);
    let b64 = CefString::from(&base64_encode(Some(html.as_bytes()))).to_string();
    format!("data:text/html;charset=utf-8;base64,{b64}")
}

/// Script that refreshes an open page with current state.
pub fn render_script(page: Page) -> String {
    format!("window.render&&render({})", state_json(page))
}

fn state_json(page: Page) -> String {
    match page {
        Page::Downloads => downloads_json(),
        Page::Settings => settings_json(),
        Page::History => history_json(),
        Page::Bookmarks => bookmarks_json(),
        Page::Passwords => passwords_json(),
    }
}

/// Search text on bare://history (set by its search box).
static HISTORY_QUERY: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn set_history_query(q: &str) {
    *HISTORY_QUERY.lock().unwrap_or_else(|e| e.into_inner()) = q.chars().take(200).collect();
}

fn history_json() -> String {
    let query = HISTORY_QUERY.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let items: Vec<String> = history::recent(&query, 500)
        .iter()
        .map(|e| format!("{{\"url\":{},\"title\":{},\"time\":{}}}", js_string(&e.url), js_string(&e.title), e.last))
        .collect();
    format!("{{\"query\":{},\"items\":[{}]}}", js_string(&query), items.join(","))
}

fn bookmarks_json() -> String {
    let items: Vec<String> = bookmarks::all()
        .iter()
        .map(|b| format!("{{\"url\":{},\"title\":{},\"folder\":{}}}", js_string(&b.url), js_string(&b.title), js_string(&b.folder)))
        .collect();
    let sources: Vec<String> = bookmarks::import_sources()
        .iter()
        .map(|(id, name, _)| format!("{{\"id\":\"{id}\",\"name\":{}}}", js_string(name)))
        .collect();
    let bar = settings::get().bookmarks_bar;
    format!(
        "{{\"items\":[{}],\"sources\":[{}],\"bar\":{bar},\"notice\":{}}}",
        items.join(","),
        sources.join(","),
        js_string(&notice())
    )
}

/// Sites and usernames only: a password is sent to the page when the user asks to see it.
fn passwords_json() -> String {
    let items: Vec<String> = passwords::all()
        .iter()
        .map(|l| format!("{{\"origin\":{},\"site\":{},\"username\":{}}}", js_string(&l.origin), js_string(passwords::site(&l.origin)), js_string(&l.username)))
        .collect();
    let never: Vec<String> = passwords::never_list().iter().map(|o| js_string(o)).collect();
    let (notice, csv) = PASSWORD_NOTICE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    format!(
        "{{\"items\":[{}],\"never\":[{}],\"offer\":{},\"notice\":{},\"csv\":{}}}",
        items.join(","),
        never.join(","),
        settings::get().save_passwords,
        js_string(&notice),
        csv.as_deref().map_or("null".into(), js_string),
    )
}

/// The result of the last password import on bare://passwords, and the imported file's name
/// while it can still be deleted from there.
static PASSWORD_NOTICE: std::sync::Mutex<(String, Option<String>)> = std::sync::Mutex::new((String::new(), None));

pub fn set_password_notice(text: &str, csv: Option<&str>) {
    *PASSWORD_NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = (text.to_string(), csv.map(String::from));
}

fn downloads_json() -> String {
    let items: Vec<String> = downloads::with(|list| {
        list.iter()
            .map(|d| {
                format!(
                    "{{\"id\":{},\"name\":{},\"url\":{},\"received\":{},\"total\":{},\"percent\":{},\"speed\":{},\"state\":\"{}\"}}",
                    d.id,
                    js_string(&d.name),
                    js_string(&d.url),
                    d.received,
                    d.total,
                    d.percent,
                    d.speed,
                    d.state.id()
                )
            })
            .collect()
    });
    format!("{{\"items\":[{}],\"folder\":{}}}", items.join(","), js_string(&downloads::folder().to_string_lossy()))
}

/// A one-line message shown at the top of the Network section (e.g. "That proxy address
/// isn't valid"). Set by the last network action.
static NOTICE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn set_notice(text: &str) {
    *NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = text.to_string();
}

fn notice() -> String {
    NOTICE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn settings_json() -> String {
    let s = settings::get();
    let engines: Vec<String> = settings::Search::ALL
        .iter()
        .map(|e| format!("{{\"id\":\"{}\",\"name\":{}}}", e.id(), js_string(e.name())))
        .collect();
    let sites: Vec<String> = s.allowed_sites.iter().map(|h| js_string(h)).collect();
    let status = update::status();
    let providers: Vec<String> = network::DNS_PROVIDERS
        .iter()
        .map(|(id, name, _)| format!("{{\"id\":\"{id}\",\"name\":{}}}", js_string(name)))
        .collect();
    let (dns, dns_custom) = match &s.dns {
        network::Dns::Off => ("off".to_string(), String::new()),
        network::Dns::Automatic => ("automatic".to_string(), String::new()),
        network::Dns::Provider(id) => (id.clone(), String::new()),
        network::Dns::Custom(url) => ("custom".to_string(), url.clone()),
    };
    let (proxy_mode, proxy_server, proxy_bypass, proxy_pac) = match &s.proxy {
        network::Proxy::System => ("system", "", "", ""),
        network::Proxy::Direct => ("direct", "", "", ""),
        network::Proxy::Manual { server, bypass } => ("manual", server.as_str(), bypass.as_str(), ""),
        network::Proxy::Pac(url) => ("pac", "", "", url.as_str()),
    };
    let net = format!(
        "\"dns\":{},\"dnsCustom\":{},\"dnsProviders\":[{}],\"proxyMode\":\"{proxy_mode}\",\"proxyServer\":{},\"proxyBypass\":{},\"proxyPac\":{},\"notice\":{},",
        js_string(&dns),
        js_string(&dns_custom),
        providers.join(","),
        js_string(proxy_server),
        js_string(proxy_bypass),
        js_string(proxy_pac),
        js_string(&notice()),
    );
    format!(
        "{{{net}\"newTabHome\":{},\"registered\":{},\"isDefault\":{},\"search\":\"{}\",\"engines\":[{}],\"blocking\":{},\"cosmetic\":{},\"social\":{},\"cookies\":{},\"allowed\":[{}],\"ask\":{},\"autoUpdate\":{},\"wipe\":{},\"version\":{},\"update\":{},\"updateReady\":{},\"updateEnabled\":{},\"engine\":{},\"engineStatus\":{},\"engineAction\":\"{}\"}}",
        s.new_tab_home,
        system::registered_as_browser(),
        system::is_default_browser(),
        s.search.id(),
        engines.join(","),
        s.blocking,
        s.cosmetic,
        s.block_social,
        s.hide_cookie_banners,
        sites.join(","),
        s.ask_download_location,
        s.auto_update,
        s.wipe_on_start,
        js_string(update::VERSION),
        js_string(&status.describe()),
        matches!(status, update::Status::Ready { .. }),
        !matches!(status, update::Status::Disabled(_)),
        js_string(crate::engine_files::chromium(&crate::engine::running())),
        js_string(&crate::engine::status().describe()),
        crate::engine::status().action(),
    )
}

const STYLE: &str = r#"
:root{--bg:#f6f6f8;--card:#fff;--fg:#1c1c1e;--muted:#6b6b72;--line:#e2e2e7;--accent:#2563eb;--danger:#c0262d;color-scheme:light dark}
@media (prefers-color-scheme:dark){:root{--bg:#18181b;--card:#232327;--fg:#ececf0;--muted:#9a9aa2;--line:#34343a;--accent:#60a5fa;--danger:#f2777b}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 "Segoe UI",system-ui,sans-serif}
main{max-width:760px;margin:0 auto;padding:32px 20px 60px}h1{font-size:22px;margin:0 0 20px}h2{font-size:15px;margin:28px 0 10px;color:var(--muted);font-weight:600}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:4px 16px}
.row{display:flex;align-items:center;gap:12px;padding:12px 0;border-top:1px solid var(--line)}.row:first-child{border-top:0}
.row .grow{flex:1;min-width:0}.sub{color:var(--muted);font-size:12.5px;overflow-wrap:anywhere}
button,select{font:inherit;color:inherit;background:transparent;border:1px solid var(--line);border-radius:6px;padding:5px 12px;cursor:pointer}
button:hover{border-color:var(--accent)}button.primary{background:var(--accent);border-color:var(--accent);color:#fff}button.danger{color:var(--danger)}
select{background:var(--card)}input[type=checkbox]{width:18px;height:18px;accent-color:var(--accent)}
.bar{height:4px;background:var(--line);border-radius:2px;margin-top:6px;overflow:hidden}.bar i{display:block;height:100%;background:var(--accent)}
.empty{color:var(--muted);padding:24px 0;text-align:center}.name{font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.top{display:flex;align-items:center;justify-content:space-between}
input[type=text]{font:inherit;color:inherit;background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:5px 8px;width:280px;max-width:45vw}
input[type=text]:focus{outline:none;border-color:var(--accent)}.notice{color:var(--danger)}
.day{font-weight:600;color:var(--muted);padding:14px 0 4px;font-size:12.5px}.time{width:56px;flex:none}a.name{color:var(--fg);text-decoration:none;display:block}a.name:hover{text-decoration:underline}
.secret{font-family:ui-monospace,Consolas,monospace;color:var(--muted);user-select:all;max-width:40%;overflow-wrap:anywhere}
.tools{display:flex;flex-wrap:wrap;gap:8px;align-items:center;margin-bottom:12px}.inline{display:flex;gap:6px;align-items:center;margin-left:auto}.notice-ok{margin-bottom:10px;color:var(--accent)}
"#;

const SCRIPT_COMMON: &str = r#"
const A=(name,params)=>{location.href=PREFIX+name+(params?'?'+new URLSearchParams(params):'')};
const el=(tag,props,...kids)=>{const e=document.createElement(tag);Object.assign(e,props||{});for(const k of kids)if(k!=null)e.append(k);return e};
const size=n=>n<1024?n+' B':n<1048576?(n/1024).toFixed(0)+' KB':n<1073741824?(n/1048576).toFixed(1)+' MB':(n/1073741824).toFixed(2)+' GB';
"#;

const HISTORY_SCRIPT: &str = r#"
function render(s){
  const q=document.getElementById('q'); if(document.activeElement!==q)q.value=s.query;
  const list=document.getElementById('list');list.replaceChildren();
  if(!s.items.length){list.append(el('div',{className:'empty',textContent:s.query?'No matches.':'No history yet.'}));return}
  let day='';
  for(const e of s.items){
    const d=new Date(e.time*1000);
    const label=d.toLocaleDateString(undefined,{weekday:'long',year:'numeric',month:'long',day:'numeric'});
    if(label!==day){day=label;list.append(el('div',{className:'day',textContent:label}))}
    const link=el('a',{href:e.url,className:'name',textContent:e.title||e.url,title:e.url});
    list.append(el('div',{className:'row'},el('div',{className:'sub time',textContent:d.toLocaleTimeString(undefined,{hour:'2-digit',minute:'2-digit'})}),
      el('div',{className:'grow'},link,el('div',{className:'sub',textContent:e.url})),
      el('button',{textContent:'Remove',onclick:()=>A('hist-delete',{url:e.url})})));
  }
}
let t=0;document.addEventListener('input',ev=>{if(ev.target.id==='q'){clearTimeout(t);t=setTimeout(()=>A('hist-search',{q:ev.target.value}),300)}});
"#;

const BOOKMARKS_SCRIPT: &str = r#"
function render(s){
  const top=document.getElementById('tools');top.replaceChildren();
  for(const src of s.sources)top.append(el('button',{textContent:'Import from '+src.name,onclick:()=>A('bm-import',{from:src.id})}));
  top.append(el('label',{className:'inline'},el('input',{type:'checkbox',checked:s.bar,onchange:e=>A('set',{key:'bookmarksBar',value:e.target.checked})}),' Show bookmarks bar'));
  const n=document.getElementById('notice');n.textContent=s.notice;n.style.display=s.notice?'block':'none';
  const list=document.getElementById('list');list.replaceChildren();
  if(!s.items.length){list.append(el('div',{className:'empty',textContent:'No bookmarks yet. Press Ctrl+D or the ☆ button to bookmark a page.'}));return}
  let folder=null;
  for(const b of [...s.items].sort((a,b)=>a.folder.localeCompare(b.folder))){
    if(b.folder!==folder){folder=b.folder;list.append(el('div',{className:'day',textContent:folder||'Bookmarks bar'}))}
    list.append(el('div',{className:'row'},
      el('div',{className:'grow'},el('a',{href:b.url,className:'name',textContent:b.title||b.url,title:b.url}),el('div',{className:'sub',textContent:b.url})),
      el('button',{textContent:'Rename',onclick:()=>{const t=prompt('Name',b.title);if(t!=null&&t.trim())A('bm-rename',{url:b.url,title:t})}}),
      el('button',{className:'danger',textContent:'Delete',onclick:()=>A('bm-delete',{url:b.url})})));
  }
}
"#;

const PASSWORDS_SCRIPT: &str = r#"
const shown=new Map(),k=(o,u)=>o+'\n'+u;let last=null;
function reveal(o,u,p){shown.set(k(o,u),p);render(last)}
function render(s){
  last=s;
  const c=el('input',{type:'checkbox',checked:s.offer,onchange:()=>A('pw-offer',{value:c.checked})});
  document.getElementById('offer').replaceChildren(el('label',{className:'row'},el('div',{className:'grow'},el('div',{textContent:'Offer to save passwords'}),
    el('div',{className:'sub',textContent:'Saved passwords are encrypted for your Windows account, only filled in when you press 🔑 in the address bar, and only shown here after Windows Hello confirms it’s you.'})),c));
  const tools=document.getElementById('tools');
  tools.replaceChildren(el('button',{textContent:'Import passwords (CSV)…',title:'A password export from Chrome, Edge, Brave, Firefox, Bitwarden, …',onclick:()=>A('pw-import')}));
  const n=document.getElementById('notice');n.textContent=s.notice;n.style.display=s.notice?'block':'none';
  if(s.csv)tools.append(el('button',{className:'danger',textContent:'Delete '+s.csv,onclick:()=>A('pw-delete-csv')}));
  const q=document.getElementById('q').value.trim().toLowerCase();
  const items=s.items.filter(i=>!q||i.site.includes(q)||i.username.toLowerCase().includes(q));
  const list=document.getElementById('list');list.replaceChildren();
  if(!items.length)list.append(el('div',{className:'empty',textContent:s.items.length?'No matches.':'No saved passwords yet. Bare offers to save one when you sign in to a site.'}));
  for(const i of items){
    const pw=shown.get(k(i.origin,i.username));
    const secret=el('span',{className:'secret',textContent:pw==null?'••••••••':pw});
    list.append(el('div',{className:'row'},
      el('div',{className:'grow'},el('div',{className:'name',textContent:i.site,title:i.origin}),el('div',{className:'sub',textContent:i.username||'(no username)'})),
      secret,
      el('button',{textContent:pw==null?'Show':'Hide',onclick:()=>{if(pw==null)A('pw-reveal',{origin:i.origin,username:i.username});else{shown.delete(k(i.origin,i.username));render(last)}}}),
      el('button',{className:'danger',textContent:'Delete',onclick:()=>{if(confirm('Delete the saved password for '+(i.username||'this login')+' on '+i.site+'?'))A('pw-delete',{origin:i.origin,username:i.username})}})));
  }
  const never=document.getElementById('never');
  never.replaceChildren(...(s.never.length?s.never.map(o=>el('div',{className:'row'},el('div',{className:'grow',textContent:o.replace(/^https?:\/\//,'')}),el('button',{textContent:'Remove',onclick:()=>A('pw-never-remove',{origin:o})}))):[el('div',{className:'row sub',textContent:'None.'})]));
}
document.addEventListener('input',ev=>{if(ev.target.id==='q')render(last)});
"#;

const DOWNLOADS_SCRIPT: &str = r#"
function render(s){
  const list=document.getElementById('list');list.replaceChildren();
  document.getElementById('folder').textContent=s.folder;
  if(!s.items.length){list.append(el('div',{className:'empty',textContent:'No downloads yet.'}));return}
  for(const d of s.items){
    const info=el('div',{className:'grow'},el('div',{className:'name',textContent:d.name,title:d.name}));
    let status;
    if(d.state==='progress'||d.state==='paused'){
      status=(d.total>0?size(d.received)+' of '+size(d.total):size(d.received))+(d.state==='paused'?' · Paused':d.speed>0?' · '+size(d.speed)+'/s':'');
      const bar=el('div',{className:'bar'},el('i'));bar.firstChild.style.width=Math.max(0,d.percent)+'%';
      info.append(el('div',{className:'sub',textContent:status}),bar);
    }else{
      status={complete:size(d.total>0?d.total:d.received),canceled:'Canceled',failed:'Failed'}[d.state];
      info.append(el('div',{className:'sub',textContent:status+' · '+d.url}));
    }
    const row=el('div',{className:'row'},info);
    if(d.state==='complete'){row.append(el('button',{textContent:'Open',onclick:()=>A('dl-open',{id:d.id})}),el('button',{textContent:'Show in folder',onclick:()=>A('dl-show',{id:d.id})}))}
    if(d.state==='progress'||d.state==='paused'){row.append(el('button',{textContent:d.state==='paused'?'Resume':'Pause',onclick:()=>A('dl-pause',{id:d.id})}),el('button',{className:'danger',textContent:'Cancel',onclick:()=>A('dl-cancel',{id:d.id})}))}
    list.append(row);
  }
}
"#;

const SETTINGS_SCRIPT: &str = r#"
const field=(value,placeholder)=>el('input',{type:'text',value:value||'',placeholder,spellcheck:false});
const row=(label,sub,...controls)=>el('div',{className:'row'},el('div',{className:'grow'},el('div',{textContent:label}),sub?el('div',{className:'sub',textContent:sub}):null),...controls);
let netDns=null,netProxy=null;
function renderNetwork(s){
  const dnsMode=netDns||s.dns, proxyMode=netProxy||s.proxyMode;
  const out=[];
  if(s.notice)out.push(el('div',{className:'row notice',textContent:s.notice}));
  const dns=el('select',{onchange:()=>{netDns=dns.value; if(dns.value==='custom'){renderNetwork(s)}else{netDns=null;A('dns',{mode:dns.value})}}});
  dns.append(el('option',{value:'automatic',textContent:'Automatic (your current provider, if it supports it)'}),el('option',{value:'off',textContent:'Off'}));
  for(const p of s.dnsProviders)dns.append(el('option',{value:p.id,textContent:p.name}));
  dns.append(el('option',{value:'custom',textContent:'Custom…'}));
  dns.value=dnsMode;
  out.push(row('Use secure DNS','Encrypts the lookups that turn site names into addresses, so people on your network can’t see or change them. If sites stop loading after choosing a provider, your network is probably blocking it: pick another one or Automatic.',dns));
  if(dnsMode==='custom'){
    const url=field(s.dnsCustom,'https://dns.example.com/dns-query');
    out.push(row('Custom provider','Must be an https:// DNS-over-HTTPS address.',url,el('button',{className:'primary',textContent:'Save',onclick:()=>{netDns=null;A('dns',{mode:'custom',url:url.value})}})));
  }
  const proxy=el('select',{onchange:()=>{netProxy=proxy.value; if(proxy.value==='manual'||proxy.value==='pac'){renderNetwork(s)}else{netProxy=null;A('proxy',{mode:proxy.value})}}});
  proxy.append(el('option',{value:'system',textContent:'Use system proxy settings'}),el('option',{value:'direct',textContent:'No proxy'}),el('option',{value:'manual',textContent:'Manual proxy'}),el('option',{value:'pac',textContent:'Automatic configuration (PAC) URL'}));
  proxy.value=proxyMode;
  out.push(row('Proxy',null,proxy));
  if(proxyMode==='manual'){
    const server=field(s.proxyServer,'socks5://127.0.0.1:1080  or  proxy.example.com:8080');
    const bypass=field(s.proxyBypass,'localhost;127.0.0.1;*.lan');
    out.push(row('Proxy server','http, https, socks4 or socks5, with a port.',server));
    out.push(row('Bypass for','Addresses that skip the proxy, separated by ;',bypass,el('button',{className:'primary',textContent:'Save',onclick:()=>{netProxy=null;A('proxy',{mode:'manual',server:server.value,bypass:bypass.value})}})));
  }
  if(proxyMode==='pac'){
    const url=field(s.proxyPac,'http://wpad.example.com/proxy.pac');
    out.push(row('Configuration URL',null,url,el('button',{className:'primary',textContent:'Save',onclick:()=>{netProxy=null;A('proxy',{mode:'pac',url:url.value})}})));
  }
  document.getElementById('network').replaceChildren(...out);
}
const toggle=(key,on,label,sub)=>{const c=el('input',{type:'checkbox',checked:on,onchange:()=>A('set',{key,value:c.checked})});
  return el('label',{className:'row'},el('div',{className:'grow'},el('div',{textContent:label}),sub?el('div',{className:'sub',textContent:sub}):null),c)};
function render(s){
  const search=el('select',{onchange:()=>A('set',{key:'search',value:search.value})});
  for(const e of s.engines)search.append(el('option',{value:e.id,textContent:e.name,selected:e.id===s.search}));
  document.getElementById('search').replaceChildren(el('div',{className:'row'},el('div',{className:'grow',textContent:'Search engine for the address bar'}),search),
    toggle('newTabHome',s.newTabHome,'New tabs open the search engine’s home page','Otherwise new tabs are blank.'));
  const sites=s.allowed.length?s.allowed.map(h=>el('div',{className:'row'},el('div',{className:'grow',textContent:h}),el('button',{textContent:'Remove',onclick:()=>A('allow-remove',{host:h})}))):[el('div',{className:'row sub',textContent:'None. Use the shield button in the toolbar to turn blocking off for a site.'})];
  document.getElementById('blocking').replaceChildren(
    toggle('blocking',s.blocking,'Block ads, trackers and malware','EasyList, EasyPrivacy, HaGeZi Pro and a malware/tracking domain list.'),
    toggle('cosmetic',s.cosmetic,'Hide leftover ad boxes','Removes empty spaces where blocked ads would have been.'),
    toggle('social',s.social,'Block social media widgets','Facebook, X, Pinterest and similar buttons, embeds and trackers on other sites. "Log in with Facebook" may stop working.'),
    toggle('cookies',s.cookies,'Hide cookie banners','Hides the cookie-consent pop-ups sites show themselves. (The consent services behind most banners, such as Cookiebot, OneTrust and TrustArc, are always blocked.) Sites then usually fall back to necessary cookies only.'));
  document.getElementById('allowed').replaceChildren(...sites);
  document.getElementById('downloads').replaceChildren(toggle('ask',s.ask,'Ask where to save each file','Otherwise files go to your Downloads folder.'));
  renderNetwork(s);
  document.getElementById('privacy').replaceChildren(
    el('div',{className:'row'},el('div',{className:'grow'},el('div',{textContent:'Clear cookies and cache'}),el('div',{className:'sub',textContent:'Signs you out of websites.'})),el('button',{className:'danger',textContent:'Clear now',onclick:()=>{if(confirm('Clear all cookies and cached files?'))A('clear-data')}})),
    toggle('wipe',s.wipe,'Erase all browsing data on next start','Cookies, cache and all site storage. Turns itself off after running.'),
    row('Passwords','Saved sign-ins, encrypted for your Windows account.',el('button',{textContent:'Manage passwords',onclick:()=>A('open-passwords')})));
  const upd=el('div',{className:'row'},el('div',{className:'grow'},el('div',{textContent:'Bare '+s.version}),el('div',{className:'sub',textContent:s.update})));
  upd.append(el('button',{textContent:'What’s new',onclick:()=>A('whats-new')}));
  if(s.updateReady)upd.append(el('button',{className:'primary',textContent:'Restart to update',onclick:()=>A('update-restart')}));
  else if(s.updateEnabled)upd.append(el('button',{textContent:'Check now',onclick:()=>A('update-check')}));
  const eng=row('Chromium engine '+s.engine,s.engineStatus);
  if(s.engineAction==='update')eng.append(el('button',{className:'primary',textContent:'Update engine',onclick:()=>A('engine-update')}));
  else if(s.engineAction==='restart')eng.append(el('button',{className:'primary',textContent:'Restart to update',onclick:()=>A('update-restart')}));
  else if(s.engineAction==='check')eng.append(el('button',{textContent:'Check now',onclick:()=>A('engine-check')}));
  const def = s.isDefault
    ? row('Default browser','Bare opens links from other apps.')
    : s.registered
      ? row('Default browser','Open links from email, chat and other apps in Bare. Windows Settings opens: choose Bare there, then Set default.',el('button',{className:'primary',textContent:'Make default',onclick:()=>A('make-default')}))
      : row('Default browser','Install Bare with BareSetup.exe to make it your default browser.');
  document.getElementById('about').replaceChildren(def,upd,eng,toggle('autoUpdate',s.autoUpdate,'Update automatically','Checks once a day. Updates are verified with a signature before installing.'));
}
"#;

fn html(page: Page) -> String {
    let (body, script) = match page {
        Page::Downloads => (
            r#"<main><div class="top"><h1>Downloads</h1><button onclick="A('dl-clear')">Clear list</button></div>
<div class="sub">Saved to <span id="folder"></span></div><h2>Files</h2><div class="card" id="list"></div></main>"#,
            DOWNLOADS_SCRIPT,
        ),
        Page::History => (
            r#"<main><div class="top"><h1>History</h1><button class="danger" onclick="if(confirm('Clear all browsing history?'))A('hist-clear')">Clear history</button></div>
<input type="text" id="q" placeholder="Search history" spellcheck="false" style="width:100%;max-width:none;margin-bottom:8px"><div class="card" id="list"></div></main>"#,
            HISTORY_SCRIPT,
        ),
        Page::Bookmarks => (
            r#"<main><h1>Bookmarks</h1><div class="tools" id="tools"></div><div class="sub notice-ok" id="notice"></div><div class="card" id="list"></div></main>"#,
            BOOKMARKS_SCRIPT,
        ),
        Page::Passwords => (
            r#"<main><h1>Passwords</h1><div class="card" id="offer"></div>
<h2>Saved passwords</h2><div class="tools" id="tools"></div><div class="sub notice-ok" id="notice"></div><input type="text" id="q" placeholder="Search passwords" spellcheck="false" style="width:100%;max-width:none;margin-bottom:8px"><div class="card" id="list"></div>
<h2>Never saved for</h2><div class="card" id="never"></div></main>"#,
            PASSWORDS_SCRIPT,
        ),
        Page::Settings => (
            r#"<main><h1>Settings</h1>
<h2>Search</h2><div class="card" id="search"></div>
<h2>Ad and tracker blocking</h2><div class="card" id="blocking"></div>
<h2>Sites where blocking is off</h2><div class="card" id="allowed"></div>
<h2>Downloads</h2><div class="card" id="downloads"></div>
<h2>Network</h2><div class="card" id="network"></div>
<h2>Privacy</h2><div class="card" id="privacy"></div>
<h2>About and updates</h2><div class="card" id="about"></div></main>"#,
            SETTINGS_SCRIPT,
        ),
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'\">\
<style>{STYLE}</style></head><body>{body}<script>const PREFIX={prefix};{SCRIPT_COMMON}{script}render({state});</script></body></html>",
        title = page.title(),
        prefix = js_string(&action_prefix()),
        state = state_json(page),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_need_the_secret() {
        let ok = format!("{}set?key=blocking&value=false", action_prefix());
        let (action, params) = parse_action(&ok).unwrap();
        assert_eq!(action, "set");
        assert_eq!(params, vec![("key".into(), "blocking".into()), ("value".into(), "false".into())]);
        assert!(is_action_url(&ok));
        assert!(parse_action("https://bare.invalid/0000/set?key=blocking&value=false").is_none());
        assert!(parse_action("https://bare.invalid/set").is_none());
        assert!(SECRET.len() >= 32);
    }

    #[test]
    fn addresses() {
        assert_eq!(Page::from_address("bare://settings"), Some(Page::Settings));
        assert_eq!(Page::from_address(" BARE://Downloads/ "), Some(Page::Downloads));
        assert_eq!(Page::from_address("bare://nope"), None);
        assert_eq!(Page::from_address("settings"), None);
    }
}
