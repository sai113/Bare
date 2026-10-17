//! CEF callbacks for pages: HTTPS-only, ad blocking, popups, downloads, built-in page actions,
//! titles, URLs and loading state.
//!
//! Most callbacks run on the UI thread and forward to `ui`. Request filtering runs on the
//! network (IO) thread, so the state it needs lives in `SHARED` behind a mutex.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use cef::*;

use crate::{block, downloads, pages, passwords, permissions, reader, settings, ui, url};

#[derive(Default)]
pub struct Shared {
    /// URL and host of the top-level page in each browser (keyed by browser id).
    pub page_url: HashMap<i32, String>,
    pub page_host: HashMap<i32, String>,
    /// Requests blocked on the current page of each browser.
    pub blocked: HashMap<i32, u32>,
    /// Hosts the user explicitly typed as http:// this session (exempt from HTTPS-only).
    pub http_ok: HashSet<String>,
    /// Token the login-capture script of each browser's current page signs its reports with.
    pub login_tokens: HashMap<i32, String>,
}

pub static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(Mutex::default);

pub fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

static REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

/// Asks the UI thread to redraw the toolbar; coalesces bursts into one redraw.
fn schedule_refresh() {
    if !REFRESH_PENDING.swap(true, Ordering::AcqRel) {
        post_task(ThreadId::UI, Some(&mut RefreshTask::new()));
    }
}

wrap_task! {
    struct RefreshTask;

    impl Task {
        fn execute(&self) {
            REFRESH_PENDING.store(false, Ordering::Release);
            ui::with_ui(|ui| ui.refresh());
        }
    }
}

fn text(s: Option<&CefString>) -> String {
    s.map(CefString::to_string).unwrap_or_default()
}

fn userfree(s: &CefStringUserfree) -> String {
    CefString::from(s).to_string()
}

pub fn client() -> Client {
    BareClient::new()
}

wrap_client! {
    struct BareClient;

    impl Client {
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(ContextMenu::new())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(Display::new())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(Downloads::new())
        }
        fn find_handler(&self) -> Option<FindHandler> {
            Some(Find::new())
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(Permissions::new())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(LifeSpan::new())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(Load::new())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(Requests::new())
        }

        /// A page's process reporting its process id (for the tab's memory use).
        fn on_process_message_received(&self, browser: Option<&mut Browser>, _frame: Option<&mut Frame>, source_process: ProcessId, message: Option<&mut ProcessMessage>) -> ::std::os::raw::c_int {
            let (Some(b), Some(m)) = (browser, message) else { return 0 };
            if source_process != ProcessId::RENDERER || CefString::from(&m.name()).to_string() != crate::app::PID_MESSAGE {
                return 0;
            }
            let Some(pid) = m.argument_list().map(|a| a.int(0)).and_then(|p| u32::try_from(p).ok()) else { return 1 };
            let id = b.identifier();
            ui::with_ui(move |ui| ui.on_renderer_pid(id, pid));
            1
        }
    }
}

wrap_display_handler! {
    struct Display;

    impl DisplayHandler {
        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            let (Some(b), title) = (browser, text(title)) else { return };
            let id = b.identifier();
            ui::with_ui(move |ui| ui.on_title(id, title));
        }

        fn on_address_change(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            let (Some(b), Some(f)) = (browser, frame) else { return };
            if f.is_main() != 1 {
                return;
            }
            let (id, url) = (b.identifier(), text(url));
            ui::with_ui(move |ui| ui.on_url(id, url));
        }

        /// The cosmetic-filter script reports the page's class names and ids through a console
        /// message; reply with the generic hide rules that match them.
        fn on_console_message(&self, browser: Option<&mut Browser>, _level: LogSeverity, message: Option<&CefString>, _source: Option<&CefString>, _line: ::std::os::raw::c_int) -> ::std::os::raw::c_int {
            let msg = text(message);
            if let Some((token, login)) = passwords::parse_report(&msg) {
                if let (Some(b), Some(login)) = (browser, login) {
                    let id = b.identifier();
                    // Only from our script in the page itself, not from frames embedded in it.
                    if shared().login_tokens.get(&id).is_some_and(|t| t == token) {
                        ui::with_ui(move |ui| ui.on_login_submitted(id, login));
                    }
                }
                return 1;
            }
            if let Some(report) = reader::parse_report(&msg) {
                if let Some(b) = browser {
                    let id = b.identifier();
                    ui::with_ui(move |ui| ui.on_reader_report(id, report));
                }
                return 1;
            }
            let Some(report) = msg.strip_prefix(block::COSMETIC_PREFIX) else { return 0 };
            let Some(b) = browser else { return 1 };
            let page = shared().page_url.get(&b.identifier()).cloned().unwrap_or_default();
            let (classes, ids) = report.split_once('\t').unwrap_or((report, ""));
            let classes: Vec<&str> = classes.split(' ').filter(|c| !c.is_empty()).take(5000).collect();
            let ids: Vec<&str> = ids.split(' ').filter(|i| !i.is_empty()).take(5000).collect();
            let (_, exceptions, generic) = block::site_cosmetics(&page);
            if generic {
                let selectors = block::generic_selectors(&classes, &ids, &exceptions);
                if let (Some(script), Some(frame)) = (block::hide_script(&selectors), b.main_frame()) {
                    frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
                }
            }
            1 // keep it out of the page's console
        }
    }
}

wrap_load_handler! {
    struct Load;

    impl LoadHandler {
        fn on_loading_state_change(&self, browser: Option<&mut Browser>, is_loading: ::std::os::raw::c_int, can_go_back: ::std::os::raw::c_int, can_go_forward: ::std::os::raw::c_int) {
            let Some(b) = browser else { return };
            let id = b.identifier();
            ui::with_ui(move |ui| ui.on_loading(id, is_loading == 1, can_go_back == 1, can_go_forward == 1));
        }

        /// Reader mode: does the page that just loaded look like an article?
        fn on_load_end(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, _http_status_code: ::std::os::raw::c_int) {
            let Some(f) = frame else { return };
            let page = userfree(&f.url());
            if f.is_main() == 1 && (page.starts_with("https://") || page.starts_with("http://")) {
                f.execute_java_script(Some(&CefString::from(reader::check_script().as_str())), None, 0);
            }
        }

        /// Cosmetic filtering (hide site-specific ad elements, and start reporting class
        /// names/ids for generic rules) and watching for logins to offer to save.
        fn on_load_start(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, _transition_type: TransitionType) {
            let (Some(b), Some(f)) = (browser, frame) else { return };
            if f.is_main() != 1 {
                return;
            }
            let page = userfree(&f.url());
            let host = url::host(&page.to_ascii_lowercase()).unwrap_or("").to_string();
            let s = settings::get();
            // Offer to save passwords typed into this page (never in a private window).
            if s.save_passwords && !crate::app::is_private() && passwords::origin(&page).is_some() {
                let token = pages::random_token();
                shared().login_tokens.insert(b.identifier(), token.clone());
                f.execute_java_script(Some(&CefString::from(passwords::capture_script(&token).as_str())), None, 0);
            }
            if host.is_empty() || !s.cosmetic || !s.blocks_on(&host) {
                return;
            }
            drop(s);
            let (selectors, _, generic) = block::site_cosmetics(&page);
            let mut script = block::hide_script(&selectors).unwrap_or_default();
            if generic {
                script.push_str(&block::observer_script());
            }
            if !script.is_empty() {
                f.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
            }
        }
    }
}

wrap_life_span_handler! {
    struct LifeSpan;

    impl LifeSpanHandler {
        /// Popups and target=_blank links open as tabs, never as new windows.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let url = text(target_url);
            ui::defer(move |ui| ui.open(&url));
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(b) = browser else { return };
            let b = b.clone();
            ui::with_ui(move |ui| ui.on_created(b));
        }

        fn do_close(&self, _browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            // Closing a tab must not close the window; closing the window lets CEF proceed.
            (!ui::is_closing()) as _
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let Some(b) = browser else { return };
            let id = b.identifier();
            {
                let mut s = shared();
                s.page_url.remove(&id);
                s.page_host.remove(&id);
                s.blocked.remove(&id);
                s.login_tokens.remove(&id);
            }
            ui::with_ui(move |ui| ui.on_closed(id));
        }
    }
}

/// Context-menu command id for "Picture in picture" (CEF's user range starts at 26500).
const MENU_PIP: ::std::os::raw::c_int = 26_501;

/// Toggles picture-in-picture: the video at (x, y) if given, else the playing (or largest)
/// video on the page.
pub fn pip_script(at: Option<(i32, i32)>) -> String {
    let (x, y) = at.unwrap_or((-1, -1));
    format!(
        "(async()=>{{try{{if(document.pictureInPictureElement){{await document.exitPictureInPicture();return}}\
const vids=[...document.querySelectorAll('video')].filter(v=>v.readyState>0&&!v.disablePictureInPicture);\
let v={x}>=0?document.elementFromPoint({x},{y})?.closest?.('video'):null;\
if(!v)v=vids.find(v=>!v.paused)||vids.sort((a,b)=>b.clientWidth*b.clientHeight-a.clientWidth*a.clientHeight)[0];\
if(v)await v.requestPictureInPicture()}}catch(e){{}}}})()"
    )
}

wrap_context_menu_handler! {
    struct ContextMenu;

    impl ContextMenuHandler {
        /// Adds "Picture in picture" when right-clicking a video.
        fn on_before_context_menu(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, params: Option<&mut ContextMenuParams>, model: Option<&mut MenuModel>) {
            let (Some(params), Some(model)) = (params, model) else { return };
            if params.media_type() == ContextMenuMediaType::VIDEO {
                if model.count() > 0 {
                    model.add_separator();
                }
                model.add_item(MENU_PIP, Some(&CefString::from("Picture in picture")));
            }
        }

        fn on_context_menu_command(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, params: Option<&mut ContextMenuParams>, command_id: ::std::os::raw::c_int, _event_flags: EventFlags) -> ::std::os::raw::c_int {
            if command_id != MENU_PIP {
                return 0;
            }
            if let Some(f) = frame {
                let at = params.map(|p| (p.xcoord(), p.ycoord()));
                f.execute_java_script(Some(&CefString::from(pip_script(at).as_str())), None, 0);
            }
            1
        }
    }
}

wrap_find_handler! {
    struct Find;

    impl FindHandler {
        fn on_find_result(&self, browser: Option<&mut Browser>, _identifier: ::std::os::raw::c_int, count: ::std::os::raw::c_int, _selection_rect: Option<&Rect>, active_match_ordinal: ::std::os::raw::c_int, final_update: ::std::os::raw::c_int) {
            let Some(b) = browser else { return };
            let id = b.identifier();
            ui::with_ui(move |ui| ui.on_find_result(id, count, active_match_ordinal, final_update == 1));
        }
    }
}

wrap_permission_handler! {
    struct Permissions;

    impl PermissionHandler {
        /// Camera and microphone (getUserMedia). Answered from the permission bar.
        fn on_request_media_access_permission(&self, browser: Option<&mut Browser>, _frame: Option<&mut Frame>, requesting_origin: Option<&CefString>, requested_permissions: u32, callback: Option<&mut MediaAccessCallback>) -> ::std::os::raw::c_int {
            let (Some(b), Some(cb)) = (browser, callback) else { return 0 };
            let grantable = permissions::grantable_media(requested_permissions);
            if grantable == 0 {
                cb.cont(0); // e.g. screen sharing, which needs a picker Bare doesn't have yet
                return 1;
            }
            let request = ui::PermissionRequest::media(text(requesting_origin), grantable, cb.clone());
            let id = b.identifier();
            ui::with_ui(move |ui| ui.request_permission(id, request));
            1
        }

        /// Location, clipboard and other prompts. Notifications are always refused.
        fn on_show_permission_prompt(&self, browser: Option<&mut Browser>, prompt_id: u64, requesting_origin: Option<&CefString>, requested_permissions: u32, callback: Option<&mut PermissionPromptCallback>) -> ::std::os::raw::c_int {
            let (Some(b), Some(cb)) = (browser, callback) else { return 0 };
            if permissions::always_denied(requested_permissions) {
                cb.cont(PermissionRequestResult::DENY);
                return 1;
            }
            let request = ui::PermissionRequest::prompt(text(requesting_origin), requested_permissions, prompt_id, cb.clone());
            let id = b.identifier();
            ui::with_ui(move |ui| ui.request_permission(id, request));
            1
        }

        /// The page went away or CEF otherwise withdrew the prompt.
        fn on_dismiss_permission_prompt(&self, browser: Option<&mut Browser>, prompt_id: u64, _result: PermissionRequestResult) {
            let Some(b) = browser else { return };
            let id = b.identifier();
            ui::with_ui(move |ui| ui.dismiss_permission(id, prompt_id));
        }
    }
}

wrap_download_handler! {
    struct Downloads;

    impl DownloadHandler {
        /// The Rust bindings default this to "no"; CEF's own default is to allow downloads.
        fn can_download(&self, _browser: Option<&mut Browser>, _url: Option<&CefString>, _request_method: Option<&CefString>) -> ::std::os::raw::c_int {
            1
        }

        fn on_before_download(&self, _browser: Option<&mut Browser>, item: Option<&mut DownloadItem>, suggested_name: Option<&CefString>, callback: Option<&mut BeforeDownloadCallback>) -> ::std::os::raw::c_int {
            let Some(cb) = callback else { return 0 };
            let folder = downloads::folder();
            let _ = std::fs::create_dir_all(&folder);
            let path = downloads::unique_path(&folder, &text(suggested_name));
            let ask = settings::get().ask_download_location;
            cb.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), ask as _);
            if let Some(item) = item {
                downloads::update(item, None);
            }
            ui::with_ui(|ui| ui.on_downloads_changed());
            1
        }

        fn on_download_updated(&self, _browser: Option<&mut Browser>, item: Option<&mut DownloadItem>, callback: Option<&mut DownloadItemCallback>) {
            let Some(item) = item else { return };
            downloads::update(item, callback);
            ui::with_ui(|ui| ui.on_downloads_changed());
        }
    }
}

wrap_request_handler! {
    struct Requests;

    impl RequestHandler {
        /// HTTPS-only for top-level navigations, and the built-in pages' action endpoint.
        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let (Some(b), Some(f), Some(r)) = (browser, frame, request) else { return 0 };
            let target = userfree(&r.url());
            if pages::is_action_url(&target) {
                // Only our own pages (data: documents holding this session's secret) may act.
                let from_page = f.is_main() == 1 && pages::is_page_url(&userfree(&f.url()));
                if let (true, Some((action, params))) = (from_page, pages::parse_action(&target)) {
                    let id = b.identifier();
                    ui::defer(move |ui| ui.page_action(id, &action, &params));
                }
                return 1;
            }
            if f.is_main() != 1 {
                return 0;
            }
            let id = b.identifier();
            let mut s = shared();
            if let Some(https) = url::upgrade(&target, &s.http_ok) {
                drop(s);
                let frame = f.clone();
                ui::defer(move |_| frame.load_url(Some(&CefString::from(https.as_str()))));
                return 1;
            }
            let host = url::host(&target.to_ascii_lowercase()).unwrap_or("").to_string();
            s.page_url.insert(id, target);
            s.page_host.insert(id, host);
            s.blocked.insert(id, 0);
            drop(s);
            schedule_refresh();
            0
        }

        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            Some(Blocker::new())
        }
    }
}

/// adblock request type for a CEF resource type.
fn request_kind(t: ResourceType) -> &'static str {
    match t {
        ResourceType::MAIN_FRAME => "document",
        ResourceType::SUB_FRAME => "subdocument",
        ResourceType::STYLESHEET => "stylesheet",
        ResourceType::SCRIPT | ResourceType::WORKER | ResourceType::SHARED_WORKER | ResourceType::SERVICE_WORKER => "script",
        ResourceType::IMAGE | ResourceType::FAVICON => "image",
        ResourceType::FONT_RESOURCE => "font",
        ResourceType::OBJECT | ResourceType::PLUGIN_RESOURCE => "object",
        ResourceType::MEDIA => "media",
        ResourceType::XHR => "xmlhttprequest",
        ResourceType::PING | ResourceType::CSP_REPORT => "ping",
        _ => "other",
    }
}

wrap_resource_request_handler! {
    struct Blocker;

    impl ResourceRequestHandler {
        /// Runs on the IO thread for every request (pages, iframes, workers). Blocked requests
        /// are cancelled before any connection is made.
        fn on_before_resource_load(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let Some(r) = request else { return ReturnValue::CONTINUE };
            let kind = request_kind(r.resource_type());
            // Never block what the user navigates to directly.
            if kind == "document" {
                return ReturnValue::CONTINUE;
            }
            let id = browser.map_or(-1, |b| b.identifier());
            let page = {
                let s = shared();
                let host = s.page_host.get(&id).map_or("", String::as_str);
                if !settings::get().blocks_on(host) {
                    return ReturnValue::CONTINUE;
                }
                s.page_url.get(&id).cloned().unwrap_or_default()
            };
            if !block::should_block(&userfree(&r.url()), &page, kind) {
                return ReturnValue::CONTINUE;
            }
            *shared().blocked.entry(id).or_default() += 1;
            schedule_refresh();
            ReturnValue::CANCEL
        }
    }
}
