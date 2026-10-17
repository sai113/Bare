//! The browser window: tab strip, navigation bar and tab contents, built with CEF Views
//! (native widgets, identical on Windows, Linux and macOS). Web pages cannot reach this UI.
//!
//! All of this runs on the CEF UI thread. CEF callbacks can re-enter while the UI is busy,
//! so work is queued through `with_ui` and runs as soon as the UI is free.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    os::raw::c_int,
    rc::Rc,
};

use cef::*;

use crate::{
    downloads, handlers, network,
    pages::{self, Page},
    permissions, settings, update, url,
};

const ICON: &[u8] = include_bytes!("../assets/bare.png");
const MAX_TAB_TITLE: usize = 24;
const ACTIVE_TAB: u32 = 0x40808080; // translucent grey, readable in light and dark themes

type Job = Box<dyn FnOnce(&mut Ui)>;

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    static JOBS: RefCell<VecDeque<Job>> = const { RefCell::new(VecDeque::new()) };
    static CLOSING: Cell<bool> = const { Cell::new(false) };
}

/// Runs `job` against the UI now, or right after the job currently running.
pub fn with_ui(job: impl FnOnce(&mut Ui) + 'static) {
    JOBS.with(|q| q.borrow_mut().push_back(Box::new(job)));
    UI.with(|cell| {
        loop {
            let Ok(mut guard) = cell.try_borrow_mut() else { return };
            let Some(job) = JOBS.with(|q| q.borrow_mut().pop_front()) else { return };
            if let Some(ui) = guard.as_mut() {
                job(ui);
            }
        }
    });
}

/// Runs `job` on a later turn of the UI message loop (outside the current CEF callback).
pub fn defer(job: impl FnOnce(&mut Ui) + 'static) {
    let job: Rc<RefCell<Option<Job>>> = Rc::new(RefCell::new(Some(Box::new(job))));
    post_task(ThreadId::UI, Some(&mut Deferred::new(job)));
}

wrap_task! {
    struct Deferred {
        job: Rc<RefCell<Option<Job>>>,
    }

    impl Task {
        fn execute(&self) {
            if let Some(job) = self.job.borrow_mut().take() {
                with_ui(job);
            }
        }
    }
}

wrap_task! {
    struct CloseWindow {
        window: Window,
    }

    impl Task {
        fn execute(&self) {
            self.window.close();
        }
    }
}

wrap_task! {
    struct UpdateTimer;

    impl Task {
        fn execute(&self) {
            update::maybe_check_automatically();
        }
    }
}

/// Removes site permissions granted earlier ("Allow" only lasts until Bare closes).
pub fn reset_session_grants(ctx: &RequestContext) {
    use permissions as p;
    let types = [
        (p::GEOLOCATION, ContentSettingTypes::GEOLOCATION),
        (p::CAMERA, ContentSettingTypes::MEDIASTREAM_CAMERA),
        (p::MIC, ContentSettingTypes::MEDIASTREAM_MIC),
        (p::CAMERA_PAN_TILT_ZOOM, ContentSettingTypes::CAMERA_PAN_TILT_ZOOM),
        (p::CLIPBOARD, ContentSettingTypes::CLIPBOARD_READ_WRITE),
        (p::MIDI_SYSEX, ContentSettingTypes::MIDI_SYSEX),
        (p::LOCAL_FONTS, ContentSettingTypes::LOCAL_FONTS),
        (p::IDLE_DETECTION, ContentSettingTypes::IDLE_DETECTION),
        (p::WINDOW_MANAGEMENT, ContentSettingTypes::WINDOW_MANAGEMENT),
        (p::MULTIPLE_DOWNLOADS, ContentSettingTypes::AUTOMATIC_DOWNLOADS),
    ];
    for (origin, mask) in permissions::take_grants() {
        let url = CefString::from(origin.as_str());
        for (bit, kind) in types {
            if mask & bit != 0 {
                ctx.set_content_setting(Some(&url), Some(&url), kind, ContentSettingValues::DEFAULT);
            }
        }
    }
}

pub fn is_closing() -> bool {
    CLOSING.get()
}

#[derive(Clone, Copy)]
enum Action {
    NewTab,
    CloseActive,
    NextTab,
    PrevTab,
    FocusAddress,
    Reload,
    Back,
    Forward,
    DevTools,
    Shield,
    Downloads,
    Settings,
    UpdateRestart,
    PermissionAllow,
    PermissionBlock,
    Find,
    FindNext,
    FindPrev,
    FindClose,
    /// For text-only buttons that act as labels.
    Nothing,
    Select(u64),
    Close(u64),
}

/// Keyboard shortcuts: (action, key code, shift, ctrl, alt). Key codes are Windows VK codes,
/// which CEF uses on every platform.
const SHORTCUTS: &[(Action, c_int, bool, bool, bool)] = &[
    (Action::NewTab, 0x54, false, true, false),       // Ctrl+T
    (Action::CloseActive, 0x57, false, true, false),  // Ctrl+W
    (Action::CloseActive, 0x73, false, true, false),  // Ctrl+F4
    (Action::NextTab, 0x09, false, true, false),      // Ctrl+Tab
    (Action::PrevTab, 0x09, true, true, false),       // Ctrl+Shift+Tab
    (Action::FocusAddress, 0x4C, false, true, false), // Ctrl+L
    (Action::FocusAddress, 0x44, false, false, true), // Alt+D
    (Action::FocusAddress, 0x75, false, false, false), // F6
    (Action::Reload, 0x74, false, false, false),      // F5
    (Action::Reload, 0x52, false, true, false),       // Ctrl+R
    (Action::Back, 0x25, false, false, true),         // Alt+Left
    (Action::Forward, 0x27, false, false, true),      // Alt+Right
    (Action::DevTools, 0x7B, false, false, false),    // F12
    (Action::Downloads, 0x4A, false, true, false),    // Ctrl+J
    (Action::Find, 0x46, false, true, false),         // Ctrl+F
    (Action::FindNext, 0x72, false, false, false),    // F3
    (Action::FindPrev, 0x72, true, false, false),     // Shift+F3
];
const SHORTCUT_BASE: c_int = 1000;
const EVENTFLAG_SHIFT_DOWN: u32 = 1 << 1;

enum Reply {
    Media(MediaAccessCallback, u32),
    Prompt(PermissionPromptCallback, u64),
}

/// A site asking for camera, microphone, location, ... waiting for the user's answer.
pub struct PermissionRequest {
    origin: String,
    mask: u32,
    reply: Reply,
}

impl PermissionRequest {
    pub fn media(origin: String, mask: u32, callback: MediaAccessCallback) -> Self {
        PermissionRequest { origin, mask, reply: Reply::Media(callback, mask) }
    }

    pub fn prompt(origin: String, mask: u32, prompt_id: u64, callback: PermissionPromptCallback) -> Self {
        PermissionRequest { origin, mask, reply: Reply::Prompt(callback, prompt_id) }
    }

    /// Remembered decisions are per site, permission set and request kind.
    fn key(&self) -> (String, u32, bool) {
        (self.origin.clone(), self.mask, matches!(self.reply, Reply::Media(..)))
    }

    fn prompt_id(&self) -> Option<u64> {
        match self.reply {
            Reply::Prompt(_, id) => Some(id),
            Reply::Media(..) => None,
        }
    }

    fn message(&self) -> String {
        let site = url::host(&self.origin.to_ascii_lowercase()).map_or(self.origin.clone(), String::from);
        let what = match self.reply {
            Reply::Media(..) => permissions::describe_media(self.mask),
            Reply::Prompt(..) => permissions::describe_prompt(self.mask),
        };
        format!("{site} wants to {what}")
    }

    fn answer(self, allow: bool) {
        match self.reply {
            Reply::Media(cb, mask) => cb.cont(if allow { mask } else { 0 }),
            Reply::Prompt(cb, _) => cb.cont(if allow { PermissionRequestResult::ACCEPT } else { PermissionRequestResult::DENY }),
        }
    }
}

struct Tab {
    id: u64,
    view: BrowserView,
    browser: Option<Browser>,
    title: String,
    url: String,
    loading: bool,
    can_back: bool,
    can_forward: bool,
    button: LabelButton,
    close: LabelButton,
    /// Set while the tab shows a built-in page (bare://downloads, bare://settings).
    page: Option<Page>,
    /// Permission requests waiting for an answer, oldest first.
    prompts: VecDeque<PermissionRequest>,
}

impl Tab {
    fn browser_id(&self) -> Option<i32> {
        self.browser.as_ref().map(|b| b.identifier())
    }

    fn label(&self) -> String {
        let title = match (self.title.trim(), self.url.as_str()) {
            ("", "") | ("", url::BLANK) | ("about:blank", _) => "New tab",
            ("", u) => u,
            (t, _) => t,
        };
        let mut label: String = title.chars().take(MAX_TAB_TITLE).collect();
        if title.chars().count() > MAX_TAB_TITLE {
            label.push('…');
        }
        if self.loading {
            label.insert_str(0, "• ");
        }
        label
    }
}

pub struct Ui {
    window: Window,
    strip: Panel,
    back: LabelButton,
    forward: LabelButton,
    address: Textfield,
    shield: LabelButton,
    downloads: LabelButton,
    update: LabelButton,
    /// Vertical layout of the window: tab strip, nav bar, then one browser view per tab (only the active one visible).
    column: BoxLayout,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    client: Client,
    context: Option<RequestContext>,
    private: bool,
    /// "<site> wants to use your camera" bar with Allow / Block.
    prompt_bar: Panel,
    prompt_text: LabelButton,
    /// Answers given this session: (origin, permissions, is media) -> allowed.
    decisions: HashMap<(String, u32, bool), bool>,
    find_bar: Panel,
    find_field: Textfield,
    find_count: LabelButton,
    /// Text of the current search, to tell a new search from cursor moves.
    find_text: String,
}

/// A button that only shows text.
fn label(text: &str) -> LabelButton {
    let b = button(text, Action::Nothing, "");
    Button::from(&b).set_ink_drop_enabled(0);
    View::from(&b).set_focusable(0);
    b
}

fn button(text: &str, action: Action, tooltip: &str) -> LabelButton {
    let mut delegate = ActionButton::new(action);
    let b = label_button_create(Some(&mut delegate), Some(&CefString::from(text))).expect("label button");
    Button::from(&b).set_tooltip_text(Some(&CefString::from(tooltip)));
    b
}

fn row(spacing: c_int) -> (Panel, Option<BoxLayout>) {
    let panel = panel_create(None).expect("panel");
    let layout = panel.set_to_box_layout(Some(&BoxLayoutSettings {
        horizontal: 1,
        between_child_spacing: spacing,
        inside_border_horizontal_spacing: 6,
        inside_border_vertical_spacing: 4,
        cross_axis_alignment: AxisAlignment::CENTER,
        ..Default::default()
    }));
    (panel, layout)
}

fn add(panel: &Panel, child: impl Into<View>) {
    let mut v: View = child.into();
    panel.add_child_view(Some(&mut v));
}

impl Ui {
    fn create(window: &Window, client: Client, context: Option<RequestContext>, private: bool) -> Ui {
        if let Some(mut icon) = image_create() {
            icon.add_png(1.0, Some(ICON));
            window.set_window_icon(Some(&mut icon));
            window.set_window_app_icon(Some(&mut icon));
        }
        let panel = Panel::from(window);
        let column = panel.set_to_box_layout(Some(&BoxLayoutSettings { horizontal: 0, ..Default::default() }));

        let (strip, _) = row(2);
        add(&strip, &button("+", Action::NewTab, "New tab (Ctrl+T)"));

        let (nav, nav_layout) = row(4);
        let back = button("←", Action::Back, "Back (Alt+Left)");
        let forward = button("→", Action::Forward, "Forward (Alt+Right)");
        let reload = button("↻", Action::Reload, "Reload (F5)");
        let address = textfield_create(Some(&mut Address::new())).expect("textfield");
        address.set_placeholder_text(Some(&CefString::from("Search or enter address")));
        let shield = button("", Action::Shield, "");
        let update = button("Update ready", Action::UpdateRestart, "Restart Bare to install the update");
        let downloads = button("↓", Action::Downloads, "Downloads (Ctrl+J)");
        let settings = button("⚙", Action::Settings, "Settings");
        View::from(&update).set_visible(0);
        for v in [
            View::from(&back),
            View::from(&forward),
            View::from(&reload),
            View::from(&address),
            View::from(&shield),
            View::from(&update),
            View::from(&downloads),
            View::from(&settings),
        ] {
            add(&nav, v);
        }
        if let Some(l) = nav_layout {
            l.set_flex_for_view(Some(&mut View::from(&address)), 1);
        }

        // Permission bar: "<site> wants to ..." [Allow] [Block]
        let (prompt_bar, prompt_layout) = row(8);
        let prompt_text = label("");
        add(&prompt_bar, &prompt_text);
        add(&prompt_bar, &button("Allow", Action::PermissionAllow, "Allow for this site until Bare closes"));
        add(&prompt_bar, &button("Block", Action::PermissionBlock, "Block for this site until Bare closes"));
        if let Some(l) = prompt_layout {
            l.set_flex_for_view(Some(&mut View::from(&prompt_text)), 1);
        }
        View::from(&prompt_text).set_background_color(0);
        prompt_text.set_horizontal_alignment(HorizontalAlignment::LEFT);
        View::from(&prompt_bar).set_background_color(0x402563EB); // translucent accent, both themes
        View::from(&prompt_bar).set_visible(0);

        // Find bar: [search field][3 of 12][↑][↓][✕]
        let (find_bar, find_layout) = row(4);
        let find_field = textfield_create(Some(&mut FindField::new())).expect("textfield");
        find_field.set_placeholder_text(Some(&CefString::from("Find in page")));
        let find_count = label("");
        for v in [
            View::from(&find_field),
            View::from(&find_count),
            View::from(&button("↑", Action::FindPrev, "Previous match (Shift+Enter)")),
            View::from(&button("↓", Action::FindNext, "Next match (Enter)")),
            View::from(&button("✕", Action::FindClose, "Close (Esc)")),
        ] {
            add(&find_bar, v);
        }
        if let Some(l) = find_layout {
            l.set_flex_for_view(Some(&mut View::from(&find_field)), 1);
        }
        View::from(&find_bar).set_visible(0);

        add(&panel, &strip);
        add(&panel, &nav);
        add(&panel, &prompt_bar);
        add(&panel, &find_bar);

        for (i, &(_, key, shift, ctrl, alt)) in SHORTCUTS.iter().enumerate() {
            window.set_accelerator(SHORTCUT_BASE + i as c_int, key, shift as _, ctrl as _, alt as _, 1);
        }

        Ui {
            window: window.clone(),
            strip,
            back,
            forward,
            address,
            shield,
            downloads,
            update,
            column: column.expect("box layout"),
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            client,
            context,
            private,
            prompt_bar,
            prompt_text,
            decisions: HashMap::new(),
            find_bar,
            find_field,
            find_count,
            find_text: String::new(),
        }
    }

    pub fn open(&mut self, target: &str) {
        let target = if target.is_empty() { url::BLANK } else { target };
        let id = self.next_id;
        self.next_id += 1;
        let mut client = self.client.clone();
        let mut delegate = TabView::new();
        let Some(view) = browser_view_create(
            Some(&mut client),
            Some(&CefString::from(target)),
            Some(&BrowserSettings::default()),
            None,
            self.context.clone().as_mut(),
            Some(&mut delegate),
        ) else {
            return;
        };
        // Our shortcuts win over the page's own key handling.
        view.set_prefer_accelerators(1);
        add(&Panel::from(&self.window), &view);
        self.column.set_flex_for_view(Some(&mut View::from(&view)), 1);

        let label = button("New tab", Action::Select(id), "");
        let close = button("✕", Action::Close(id), "Close tab (Ctrl+W)");
        let at = self.strip.child_view_count() as c_int - 1; // before the "+" button
        self.strip.add_child_view_at(Some(&mut View::from(&label)), at);
        self.strip.add_child_view_at(Some(&mut View::from(&close)), at + 1);

        self.close_find();
        if let Some(old) = self.tabs.get(self.active) {
            View::from(&old.view).set_visible(0);
        }
        self.tabs.push(Tab {
            id,
            view,
            browser: None,
            title: String::new(),
            url: if target == url::BLANK { String::new() } else { target.to_string() },
            loading: true,
            can_back: false,
            can_forward: false,
            button: label,
            close,
            page: None,
            prompts: VecDeque::new(),
        });
        self.active = self.tabs.len() - 1;
        Panel::from(&self.window).layout();
        self.refresh();
        if target == url::BLANK { self.focus_address() } else { self.focus_page() }
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn tab_for_browser(&mut self, browser_id: i32) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.browser_id() == Some(browser_id))
    }

    fn select(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.close_find();
        if let Some(old) = self.tabs.get(self.active) {
            View::from(&old.view).set_visible(0);
        }
        self.active = index;
        View::from(&self.tabs[index].view).set_visible(1);
        Panel::from(&self.window).layout();
        self.refresh();
        self.focus_page();
    }

    fn close(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            // Closing the last tab closes the window.
            self.close_window();
            return;
        }
        if index == self.active {
            self.close_find();
        }
        // A window close that was cancelled (e.g. a "Leave this page?" dialog) must not leave
        // Bare thinking the window is closing, or this tab close would close the window.
        CLOSING.set(false);
        let mut tab = self.tabs.remove(index);
        for request in tab.prompts.drain(..) {
            request.answer(false);
        }
        self.strip.remove_child_view(Some(&mut View::from(&tab.button)));
        self.strip.remove_child_view(Some(&mut View::from(&tab.close)));
        if let Some(host) = tab.browser.as_ref().and_then(|b| b.host()) {
            host.close_browser(1);
        }
        Panel::from(&self.window).remove_child_view(Some(&mut View::from(&tab.view)));
        if self.active > index || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        View::from(&self.tabs[self.active].view).set_visible(1);
        Panel::from(&self.window).layout();
        self.refresh();
        self.focus_page();
    }

    /// Closes the window once the current UI work has finished: CEF asks `can_close` right
    /// away, and that check needs the UI state this method is still holding.
    fn close_window(&self) {
        post_task(ThreadId::UI, Some(&mut CloseWindow::new(self.window.clone())));
    }

    fn act(&mut self, action: Action) {
        let n = self.tabs.len();
        let browser = self.tabs.get(self.active).and_then(|t| t.browser.clone());
        match action {
            Action::NewTab => self.open(url::BLANK),
            Action::CloseActive => self.close(self.active),
            Action::NextTab if n > 0 => self.select((self.active + 1) % n),
            Action::PrevTab if n > 0 => self.select((self.active + n - 1) % n),
            Action::FocusAddress => self.focus_address(),
            Action::Reload | Action::Back | Action::Forward | Action::DevTools => {
                let Some(b) = browser else { return };
                match action {
                    Action::Reload => b.reload(),
                    Action::Back => b.go_back(),
                    Action::Forward => b.go_forward(),
                    _ => {
                        if let Some(host) = b.host() {
                            host.show_dev_tools(None, None, None, None);
                        }
                    }
                }
            }
            Action::Shield => self.toggle_shield(),
            Action::Downloads => self.open_page(Page::Downloads),
            Action::Settings => self.open_page(Page::Settings),
            Action::UpdateRestart => {
                update::request_restart();
                self.close_window();
            }
            Action::PermissionAllow => self.answer_permission(true),
            Action::PermissionBlock => self.answer_permission(false),
            Action::Find => self.open_find(),
            Action::FindNext => self.find_step(true),
            Action::FindPrev => self.find_step(false),
            Action::FindClose => {
                self.close_find();
                self.focus_page();
            }
            Action::Select(id) => {
                if let Some(i) = self.index_of(id) {
                    self.select(i);
                }
            }
            Action::Close(id) => {
                if let Some(i) = self.index_of(id) {
                    self.close(i);
                }
            }
            _ => {}
        }
    }

    /// Turns the blocker off (or back on) for the current site, then reloads it. Remembered.
    fn toggle_shield(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(id) = tab.browser_id() else { return };
        let Some(host) = handlers::shared().page_host.get(&id).cloned().filter(|h| !h.is_empty()) else { return };
        settings::update(|s| {
            if !s.allowed_sites.remove(&host) {
                s.allowed_sites.insert(host);
            }
        });
        if let Some(b) = &tab.browser {
            b.reload();
        }
        self.refresh();
        self.refresh_pages(Page::Settings);
    }

    /// Shows a built-in page, reusing its tab if one is already open.
    fn open_page(&mut self, page: Page) {
        if let Some(i) = self.tabs.iter().position(|t| t.page == Some(page)) {
            self.select(i);
            self.refresh_pages(page);
            return;
        }
        let blank = self.tabs.get(self.active).is_some_and(|t| t.url.is_empty() && t.page.is_none() && t.browser.is_some());
        if blank {
            // Reuse an empty new tab.
            let tab = &mut self.tabs[self.active];
            tab.page = Some(page);
            tab.url = page.display_url();
            if let Some(frame) = tab.browser.as_ref().and_then(|b| b.main_frame()) {
                frame.load_url(Some(&CefString::from(pages::data_url(page).as_str())));
            }
        } else {
            self.open(&pages::data_url(page));
            if let Some(tab) = self.tabs.last_mut() {
                tab.page = Some(page);
                tab.url = page.display_url();
            }
        }
        self.refresh();
        self.focus_page();
    }

    /// Re-renders every open copy of a built-in page with current state.
    fn refresh_pages(&self, page: Page) {
        let script = pages::render_script(page);
        for t in self.tabs.iter().filter(|t| t.page == Some(page)) {
            if let Some(frame) = t.browser.as_ref().and_then(|b| b.main_frame()) {
                frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
            }
        }
    }

    /// A button on a built-in page was pressed (already verified by `handlers`).
    pub fn page_action(&mut self, browser_id: i32, action: &str, params: &[(String, String)]) {
        let page = self.tabs.iter().find(|t| t.browser_id() == Some(browser_id)).and_then(|t| t.page);
        let param = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str()).unwrap_or("");
        let id = || param("id").parse::<u32>().unwrap_or(0);
        match (page, action) {
            (Some(Page::Downloads), "dl-open") => downloads::open(id()),
            (Some(Page::Downloads), "dl-show") => downloads::show_in_folder(id()),
            (Some(Page::Downloads), "dl-cancel") => downloads::cancel(id()),
            (Some(Page::Downloads), "dl-pause") => downloads::pause_or_resume(id()),
            (Some(Page::Downloads), "dl-clear") => downloads::clear_finished(),
            (Some(Page::Settings), "set") => {
                let (key, value) = (param("key"), param("value"));
                let on = value == "true";
                settings::update(|s| match key {
                    "search" => s.search = settings::Search::from_id(value).unwrap_or(s.search),
                    "blocking" => s.blocking = on,
                    "cosmetic" => s.cosmetic = on,
                    "social" => s.block_social = on,
                    "cookies" => s.hide_cookie_banners = on,
                    "ask" => s.ask_download_location = on,
                    "autoUpdate" => s.auto_update = on,
                    "wipe" => s.wipe_on_start = on,
                    _ => {}
                });
                if matches!(key, "social" | "cookies") {
                    crate::block::reload(settings::get().extras());
                }
            }
            (Some(Page::Settings), "dns") => {
                let dns = match param("mode") {
                    "custom" if network::valid_doh(param("url").trim()) => Some(network::Dns::Custom(param("url").trim().into())),
                    "custom" => None,
                    mode => Some(network::Dns::parse(mode)),
                };
                match dns.map(|d| (network::apply_dns(&d), d)) {
                    Some((Ok(()), dns)) => {
                        settings::update(|s| s.dns = dns);
                        pages::set_notice("");
                    }
                    Some((Err(e), _)) => pages::set_notice(&format!("Couldn\u{2019}t change secure DNS ({e}).")),
                    None => pages::set_notice("That isn\u{2019}t a valid secure DNS address. It must start with https://."),
                }
            }
            (Some(Page::Settings), "proxy") => {
                let (server, bypass, url) = (param("server").trim(), param("bypass").trim(), param("url").trim());
                let proxy = match param("mode") {
                    "direct" => Some(network::Proxy::Direct),
                    "manual" if network::valid_proxy_server(server) => {
                        Some(network::Proxy::Manual { server: server.into(), bypass: bypass.into() })
                    }
                    "pac" if network::valid_pac(url) => Some(network::Proxy::Pac(url.into())),
                    "manual" | "pac" => None,
                    _ => Some(network::Proxy::System),
                };
                let applied = proxy.as_ref().map(|p| match self.context.as_ref() {
                    Some(ctx) => network::apply_proxy(ctx, p),
                    None => Err("no browser profile".into()),
                });
                match (proxy, applied) {
                    (Some(p), Some(Ok(()))) => {
                        settings::update(|s| s.proxy = p);
                        pages::set_notice("");
                    }
                    (Some(_), Some(Err(e))) => pages::set_notice(&format!("Couldn\u{2019}t change the proxy ({e}).")),
                    (Some(_), None) => {}
                    (None, _) if param("mode") == "manual" => {
                        pages::set_notice("That proxy address isn\u{2019}t valid. Use host:port, optionally with http://, https://, socks4:// or socks5://.")
                    }
                    (None, _) => pages::set_notice("That configuration URL isn\u{2019}t valid."),
                }
            }
            (Some(Page::Settings), "allow-remove") => {
                let host = param("host").to_string();
                settings::update(|s| {
                    s.allowed_sites.remove(&host);
                });
            }
            (Some(Page::Settings), "clear-data") => self.clear_cookies_and_cache(),
            (Some(Page::Settings), "update-check") => update::check(),
            (Some(Page::Settings), "update-restart") => self.act(Action::UpdateRestart),
            _ => return,
        }
        if let Some(page) = page {
            self.refresh_pages(page);
        }
        self.refresh();
    }

    fn clear_cookies_and_cache(&self) {
        let Some(ctx) = self.context.as_ref() else { return };
        if let Some(cookies) = ctx.cookie_manager(None) {
            cookies.delete_cookies(None, None, None);
        }
        ctx.clear_http_cache(None);
    }

    pub fn on_downloads_changed(&mut self) {
        self.refresh_pages(Page::Downloads);
        self.refresh();
    }

    pub fn on_update_status(&mut self) {
        self.refresh_pages(Page::Settings);
        self.refresh();
    }

    fn navigate(&mut self, input: &str) {
        if let Some(page) = Page::from_address(input) {
            return self.open_page(page);
        }
        let target = url::resolve(input, settings::get().search.prefix());
        if input.trim_start().to_ascii_lowercase().starts_with("http://") {
            if let Some(host) = url::host(&target.to_ascii_lowercase()) {
                handlers::shared().http_ok.insert(host.to_string());
            }
        }
        if let Some(frame) = self.tabs.get(self.active).and_then(|t| t.browser.as_ref()).and_then(|b| b.main_frame()) {
            frame.load_url(Some(&CefString::from(target.as_str())));
        }
        self.focus_page();
    }

    fn focus_page(&self) {
        if let Some(t) = self.tabs.get(self.active) {
            View::from(&t.view).request_focus();
        }
    }

    fn focus_address(&self) {
        View::from(&self.address).request_focus();
        self.address.select_all(0);
    }

    /// Redraws tab labels, the address bar, buttons and the shield from current state.
    pub fn refresh(&mut self) {
        let Some(active) = self.tabs.get(self.active) else { return };
        let browser_id = active.browser_id().unwrap_or(-1);
        let (host, blocked) = {
            let s = handlers::shared();
            let host = if active.page.is_some() { String::new() } else { s.page_host.get(&browser_id).cloned().unwrap_or_default() };
            (host, s.blocked.get(&browser_id).copied().unwrap_or(0))
        };
        let (blocking, off) = {
            let s = settings::get();
            (s.blocking, s.allowed_sites.contains(&host))
        };

        for (i, t) in self.tabs.iter().enumerate() {
            t.button.set_text(Some(&CefString::from(t.label().as_str())));
            Button::from(&t.button).set_tooltip_text(Some(&CefString::from(t.title.as_str())));
            View::from(&t.button).set_background_color(if i == self.active { ACTIVE_TAB } else { 0 });
        }

        if View::from(&self.address).has_focus() != 1 {
            self.address.set_text(Some(&CefString::from(active.url.as_str())));
        }
        View::from(&self.back).set_enabled(active.can_back as _);
        View::from(&self.forward).set_enabled(active.can_forward as _);

        let shield = View::from(&self.shield);
        shield.set_visible((!host.is_empty() && blocking) as _);
        let (label, tip) = if off {
            ("Blocking off".to_string(), "Ad and tracker blocking is off for this site. Click to turn it back on.")
        } else {
            (format!("{blocked} blocked"), "Ads and trackers blocked on this page. Click to turn blocking off for this site.")
        };
        self.shield.set_text(Some(&CefString::from(label.as_str())));
        Button::from(&self.shield).set_tooltip_text(Some(&CefString::from(tip)));

        // Download indicator: "↓", "↓ 42%" for one download, "↓ 3" for several.
        let active_downloads: Vec<i32> = downloads::with(|list| {
            list.iter()
                .filter(|d| matches!(d.state, downloads::State::InProgress | downloads::State::Paused))
                .map(|d| d.percent)
                .collect()
        });
        let label = match active_downloads.as_slice() {
            [] => "↓".to_string(),
            [p] if *p >= 0 => format!("↓ {p}%"),
            list => format!("↓ {}", list.len()),
        };
        self.downloads.set_text(Some(&CefString::from(label.as_str())));
        View::from(&self.update).set_visible(matches!(update::status(), update::Status::Ready { .. }) as _);

        let mode = if self.private { " (Private)" } else { "" };
        let title = format!("{} - Bare{mode}", active.label().trim_start_matches("• "));
        self.window.set_title(Some(&CefString::from(title.as_str())));
        self.refresh_prompt_bar();
    }

    // ---- events from page handlers ----

    pub fn on_created(&mut self, browser: Browser) {
        let mut b = browser.clone();
        let Some(view) = browser_view_get_for_browser(Some(&mut b)) else { return };
        let mut v = View::from(&view);
        if let Some(tab) = self.tabs.iter_mut().find(|t| View::from(&t.view).is_same(Some(&mut v)) == 1) {
            tab.browser = Some(browser);
        }
        self.refresh();
    }

    pub fn on_title(&mut self, browser_id: i32, title: String) {
        if let Some(t) = self.tab_for_browser(browser_id) {
            t.title = title;
        }
        self.refresh();
    }

    pub fn on_url(&mut self, browser_id: i32, url: String) {
        if let Some(t) = self.tab_for_browser(browser_id) {
            // Requests from a site the tab has left are no longer relevant.
            let host = |u: &str| url::host(&u.to_ascii_lowercase()).map(String::from);
            if host(&t.url) != host(&url) {
                for request in t.prompts.drain(..) {
                    request.answer(false);
                }
            }
            match t.page {
                // Built-in pages show their bare:// address, not the data: URL.
                Some(page) if pages::is_page_url(&url) => t.url = page.display_url(),
                _ => {
                    t.page = None;
                    t.url = if url == url::BLANK || pages::is_page_url(&url) { String::new() } else { url };
                }
            }
        }
        self.refresh();
    }

    pub fn on_loading(&mut self, browser_id: i32, loading: bool, back: bool, forward: bool) {
        if let Some(t) = self.tab_for_browser(browser_id) {
            (t.loading, t.can_back, t.can_forward) = (loading, back, forward);
        }
        self.refresh();
    }

    pub fn on_closed(&mut self, browser_id: i32) {
        let Some(i) = self.tabs.iter().position(|t| t.browser_id() == Some(browser_id)) else { return };
        if is_closing() {
            for request in self.tabs.remove(i).prompts {
                request.answer(false);
            }
        } else {
            // Closed by the page itself (window.close()).
            self.close(i);
        }
    }

    // ---- permission requests ----

    pub fn request_permission(&mut self, browser_id: i32, request: PermissionRequest) {
        if let Some(&allowed) = self.decisions.get(&request.key()) {
            return request.answer(allowed);
        }
        match self.tab_for_browser(browser_id) {
            Some(tab) => tab.prompts.push_back(request),
            None => return request.answer(false),
        }
        self.refresh_prompt_bar();
    }

    /// CEF withdrew a prompt (e.g. the page navigated away); its callback is no longer valid.
    pub fn dismiss_permission(&mut self, browser_id: i32, prompt_id: u64) {
        if let Some(tab) = self.tab_for_browser(browser_id) {
            tab.prompts.retain(|r| r.prompt_id() != Some(prompt_id));
        }
        self.refresh_prompt_bar();
    }

    fn answer_permission(&mut self, allow: bool) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let Some(request) = tab.prompts.pop_front() else { return };
        let key = request.key();
        if request.prompt_id().is_some() && !self.private {
            // Chromium would keep this answer forever; note it so it can be reset on close.
            permissions::record_grant(&request.origin, request.mask);
        }
        request.answer(allow);
        // Answer any identical requests already queued in other tabs too.
        for t in &mut self.tabs {
            let (same, rest): (VecDeque<_>, VecDeque<_>) = t.prompts.drain(..).partition(|r| r.key() == key);
            t.prompts = rest;
            for r in same {
                r.answer(allow);
            }
        }
        self.decisions.insert(key, allow);
        self.refresh_prompt_bar();
    }

    /// Shows the active tab's oldest pending request, if any.
    fn refresh_prompt_bar(&self) {
        let message = self.tabs.get(self.active).and_then(|t| t.prompts.front()).map(PermissionRequest::message);
        View::from(&self.prompt_bar).set_visible(message.is_some() as _);
        if let Some(m) = message {
            self.prompt_text.set_text(Some(&CefString::from(m.as_str())));
        }
        Panel::from(&self.window).layout();
    }

    // ---- find in page ----

    fn active_host(&self) -> Option<BrowserHost> {
        self.tabs.get(self.active).and_then(|t| t.browser.as_ref()).and_then(|b| b.host())
    }

    fn open_find(&mut self) {
        View::from(&self.find_bar).set_visible(1);
        Panel::from(&self.window).layout();
        View::from(&self.find_field).request_focus();
        self.find_field.select_all(0);
        if !self.find_text.is_empty() {
            self.find_step(true);
        }
    }

    fn close_find(&mut self) {
        if View::from(&self.find_bar).is_visible() != 1 {
            return;
        }
        if let Some(host) = self.active_host() {
            host.stop_finding(1);
        }
        self.find_text.clear();
        self.find_field.set_text(Some(&CefString::from("")));
        self.find_count.set_text(Some(&CefString::from("")));
        View::from(&self.find_bar).set_visible(0);
        Panel::from(&self.window).layout();
    }

    /// The search text changed: start a new search from the top.
    fn find_changed(&mut self) {
        let text = CefString::from(&self.find_field.text()).to_string();
        if text == self.find_text {
            return; // cursor moved, selection changed, ...
        }
        self.find_text = text;
        let Some(host) = self.active_host() else { return };
        if self.find_text.is_empty() {
            host.stop_finding(1);
            self.find_count.set_text(Some(&CefString::from("")));
        } else {
            host.find(Some(&CefString::from(self.find_text.as_str())), 1, 0, 0);
        }
    }

    fn find_step(&mut self, forward: bool) {
        if View::from(&self.find_bar).is_visible() != 1 {
            return self.open_find();
        }
        if self.find_text.is_empty() {
            return;
        }
        if let Some(host) = self.active_host() {
            host.find(Some(&CefString::from(self.find_text.as_str())), forward as _, 0, 1);
        }
    }

    pub fn on_find_result(&mut self, browser_id: i32, count: c_int, active: c_int, _final_update: bool) {
        let is_active = self.tabs.get(self.active).and_then(Tab::browser_id) == Some(browser_id);
        if !is_active || self.find_text.is_empty() {
            return;
        }
        let text = find_label(count, active);
        self.find_count.set_text(Some(&CefString::from(text.as_str())));
        self.find_bar.layout();
    }
}

wrap_button_delegate! {
    struct ActionButton {
        action: Action,
    }

    impl ViewDelegate {}

    impl ButtonDelegate {
        fn on_button_pressed(&self, _button: Option<&mut Button>) {
            // Run after Chromium has finished handling the click: actions like closing a tab
            // destroy this very button, and doing that mid-click crashes Chromium.
            let action = self.action;
            defer(move |ui| ui.act(action));
        }
    }
}

wrap_textfield_delegate! {
    struct Address {}

    impl ViewDelegate {}

    impl TextfieldDelegate {
        fn on_key_event(&self, textfield: Option<&mut Textfield>, event: Option<&KeyEvent>) -> c_int {
            let (Some(field), Some(e)) = (textfield, event) else { return 0 };
            if e.type_ != KeyEventType::RAWKEYDOWN {
                return 0;
            }
            match e.windows_key_code {
                0x0D => {
                    // Enter
                    let input = CefString::from(&field.text()).to_string();
                    with_ui(move |ui| ui.navigate(&input));
                    1
                }
                0x1B => {
                    // Escape: restore the page's URL
                    with_ui(|ui| {
                        if let Some(t) = ui.tabs.get(ui.active) {
                            ui.address.set_text(Some(&CefString::from(t.url.as_str())));
                        }
                        ui.focus_page();
                    });
                    1
                }
                _ => 0,
            }
        }
    }
}

wrap_textfield_delegate! {
    struct FindField {}

    impl ViewDelegate {}

    impl TextfieldDelegate {
        fn on_key_event(&self, _textfield: Option<&mut Textfield>, event: Option<&KeyEvent>) -> c_int {
            let Some(e) = event else { return 0 };
            if e.type_ != KeyEventType::RAWKEYDOWN {
                return 0;
            }
            match e.windows_key_code {
                0x0D => {
                    // Enter: next match; Shift+Enter: previous
                    let forward = e.modifiers & EVENTFLAG_SHIFT_DOWN == 0;
                    with_ui(move |ui| ui.find_step(forward));
                    1
                }
                0x1B => {
                    with_ui(|ui| ui.act(Action::FindClose));
                    1
                }
                _ => 0,
            }
        }

        /// Search as you type.
        fn on_after_user_action(&self, _textfield: Option<&mut Textfield>) {
            with_ui(|ui| ui.find_changed());
        }
    }
}

wrap_browser_view_delegate! {
    struct TabView {}

    impl ViewDelegate {}

    impl BrowserViewDelegate {
        fn chrome_toolbar_type(&self, _browser_view: Option<&mut BrowserView>) -> ChromeToolbarType {
            ChromeToolbarType::NONE
        }

        /// Alloy style: a plain embedded browser without Chrome's app features (sign-in,
        /// password manager, ...). Unlike Chrome style it allows one browser view per tab in
        /// the same window.
        fn browser_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::ALLOY
        }
    }
}

wrap_window_delegate! {
    pub struct MainWindow {
        client: Client,
        context: Option<RequestContext>,
        start: String,
        private: bool,
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size { width: 1280, height: 800 }
        }
    }

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let Some(window) = window else { return };
            let ui = Ui::create(window, self.client.clone(), self.context.clone(), self.private);
            UI.with(|cell| *cell.borrow_mut() = Some(ui));
            let start = self.start.clone();
            with_ui(move |ui| match Page::from_address(&start) {
                Some(page) => ui.open_page(page),
                None => ui.open(&start),
            });
            window.show();
            // Check for updates shortly after startup (at most once a day, if enabled).
            post_delayed_task(ThreadId::UI, Some(&mut UpdateTimer::new()), 15_000);
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            if let Some(ctx) = self.context.as_ref() {
                reset_session_grants(ctx);
            }
            UI.with(|cell| {
                if let Ok(mut ui) = cell.try_borrow_mut() {
                    ui.take();
                }
            });
            quit_message_loop();
        }

        /// The window closes once every tab's browser agrees to close (beforeunload etc.).
        fn can_close(&self, window: Option<&mut Window>) -> c_int {
            CLOSING.set(true);
            UI.with(|cell| {
                let Ok(ui) = cell.try_borrow() else {
                    // UI busy: try again once it's free.
                    if let Some(w) = window {
                        post_task(ThreadId::UI, Some(&mut CloseWindow::new(w.clone())));
                    }
                    return 0;
                };
                let Some(ui) = ui.as_ref() else { return 1 };
                let mut all = 1;
                for t in &ui.tabs {
                    if let Some(host) = t.browser.as_ref().and_then(|b| b.host()) {
                        if host.try_close_browser() != 1 {
                            all = 0;
                        }
                    }
                }
                all
            })
        }

        fn window_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::ALLOY
        }

        fn can_resize(&self, _window: Option<&mut Window>) -> c_int {
            1
        }

        fn can_maximize(&self, _window: Option<&mut Window>) -> c_int {
            1
        }

        fn can_minimize(&self, _window: Option<&mut Window>) -> c_int {
            1
        }

        fn on_accelerator(&self, _window: Option<&mut Window>, command_id: c_int) -> c_int {
            let Some(&(action, ..)) = SHORTCUTS.get((command_id - SHORTCUT_BASE) as usize) else { return 0 };
            with_ui(move |ui| ui.act(action));
            1
        }
    }
}

/// Find bar counter. Chromium only reports which match is active once the user steps
/// through them, so a fresh search shows the total.
fn find_label(count: c_int, active: c_int) -> String {
    match (count, active) {
        (0, _) => "No results".to_string(),
        (1, a) if a <= 0 => "1 match".to_string(),
        (n, a) if a <= 0 => format!("{n} matches"),
        (n, a) => format!("{a} of {n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_counter() {
        assert_eq!(find_label(0, 0), "No results");
        assert_eq!(find_label(1, 0), "1 match");
        assert_eq!(find_label(2, 0), "2 matches");
        assert_eq!(find_label(2, 1), "1 of 2");
        assert_eq!(find_label(12, 12), "12 of 12");
    }
}
