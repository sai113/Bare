//! The browser window: tab strip, navigation bar and tab contents, built with CEF Views
//! (native widgets, identical on Windows, Linux and macOS). Web pages cannot reach this UI.
//!
//! All of this runs on the CEF UI thread. CEF callbacks can re-enter while the UI is busy,
//! so work is queued through `with_ui` and runs as soon as the UI is free.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    os::raw::c_int,
    rc::Rc,
};

use cef::*;

use crate::{
    bookmarks, downloads, handlers, history, memory, network,
    pages::{self, Page},
    passwords, permissions, reader, settings, update, url,
};

const ICON: &[u8] = include_bytes!("../assets/bare.png");
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

/// Like `defer`, but after `ms` milliseconds.
pub fn defer_after(ms: i64, job: impl FnOnce(&mut Ui) + 'static) {
    let job: Rc<RefCell<Option<Job>>> = Rc::new(RefCell::new(Some(Box::new(job))));
    post_delayed_task(ThreadId::UI, Some(&mut Deferred::new(job)), ms);
}

/// UI work produced on another thread.
type SendJob = Box<dyn FnOnce(&mut Ui) + Send>;

/// Runs `work` on a new thread (for things that block, like a Windows Hello prompt); the job it
/// returns then runs on the UI thread.
pub fn background(work: impl FnOnce() -> SendJob + Send + 'static) {
    std::thread::spawn(move || {
        let job = work();
        post_task(ThreadId::UI, Some(&mut UiJob::new(std::sync::Arc::new(std::sync::Mutex::new(Some(job))))));
    });
}

wrap_task! {
    struct UiJob {
        job: std::sync::Arc<std::sync::Mutex<Option<SendJob>>>,
    }

    impl Task {
        fn execute(&self) {
            if let Some(job) = self.job.lock().unwrap_or_else(|e| e.into_inner()).take() {
                with_ui(job);
            }
        }
    }
}

/// Chromium's zoom level is a power of 1.2 (level 0 = 100%, 1 = 120%, -1 ≈ 83%).
fn zoom_percent(level: f64) -> i32 {
    (100.0 * 1.2f64.powf(level)).round() as i32
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

/// How often tab memory use is re-read.
const MEMORY_SAMPLE_MS: i64 = 5_000;

wrap_task! {
    struct MemoryTimer;

    impl Task {
        fn execute(&self) {
            with_ui(|ui| ui.sample_memory());
            if !is_closing() {
                post_delayed_task(ThreadId::UI, Some(&mut MemoryTimer::new()), MEMORY_SAMPLE_MS);
            }
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
    /// 🏠: the search engine's home page in the current tab.
    Home,
    Back,
    Forward,
    DevTools,
    Shield,
    Downloads,
    Settings,
    UpdateRestart,
    PermissionAllow,
    PermissionBlock,
    PictureInPicture,
    HistoryPage,
    BookmarksPage,
    ToggleBookmark,
    ToggleBookmarksBar,
    OpenBookmark(usize),
    BookmarkFolder(usize),
    PickSuggestion(usize),
    BookmarksMore,
    TabMenu,
    SelectTab(usize),
    LastTab,
    MoveTabLeft,
    MoveTabRight,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Find,
    FindNext,
    FindPrev,
    FindClose,
    Reader,
    /// 🔑: fill a saved login (a menu when there are several).
    Logins,
    SaveLogin,
    NeverSaveLogin,
    DismissLogin,
    /// For text-only buttons that act as labels.
    Nothing,
    Select(u64),
    Close(u64),
    /// 🔊 / 🔇 on a tab: mute or unmute it.
    ToggleMute(u64),
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
    (Action::Home, 0x24, false, false, true),         // Alt+Home
    (Action::Back, 0x25, false, false, true),         // Alt+Left
    (Action::Forward, 0x27, false, false, true),      // Alt+Right
    (Action::DevTools, 0x7B, false, false, false),    // F12
    (Action::Downloads, 0x4A, false, true, false),    // Ctrl+J
    (Action::Find, 0x46, false, true, false),         // Ctrl+F
    (Action::FindNext, 0x72, false, false, false),    // F3
    (Action::FindPrev, 0x72, true, false, false),     // Shift+F3
    (Action::PictureInPicture, 0x50, false, false, true), // Alt+P
    (Action::Reader, 0x78, false, false, false),         // F9
    (Action::HistoryPage, 0x48, false, true, false),       // Ctrl+H
    (Action::BookmarksPage, 0x4F, true, true, false),      // Ctrl+Shift+O
    (Action::ToggleBookmark, 0x44, false, true, false),    // Ctrl+D
    (Action::ToggleBookmarksBar, 0x42, true, true, false), // Ctrl+Shift+B
    (Action::SelectTab(0), 0x31, false, true, false),  // Ctrl+1 … Ctrl+8
    (Action::SelectTab(1), 0x32, false, true, false),
    (Action::SelectTab(2), 0x33, false, true, false),
    (Action::SelectTab(3), 0x34, false, true, false),
    (Action::SelectTab(4), 0x35, false, true, false),
    (Action::SelectTab(5), 0x36, false, true, false),
    (Action::SelectTab(6), 0x37, false, true, false),
    (Action::SelectTab(7), 0x38, false, true, false),
    (Action::LastTab, 0x39, false, true, false),      // Ctrl+9
    (Action::MoveTabLeft, 0x21, true, true, false),   // Ctrl+Shift+PageUp
    (Action::MoveTabRight, 0x22, true, true, false),  // Ctrl+Shift+PageDown
    (Action::ZoomIn, 0xBB, false, true, false),       // Ctrl+=
    (Action::ZoomIn, 0xBB, true, true, false),        // Ctrl++ (Shift+=)
    (Action::ZoomIn, 0x6B, false, true, false),       // Ctrl+Numpad+
    (Action::ZoomOut, 0xBD, false, true, false),      // Ctrl+-
    (Action::ZoomOut, 0x6D, false, true, false),      // Ctrl+Numpad-
    (Action::ZoomReset, 0x30, false, true, false),    // Ctrl+0
    (Action::ZoomReset, 0x60, false, true, false),    // Ctrl+Numpad0
];
const SHORTCUT_BASE: c_int = 1000;
const EVENTFLAG_SHIFT_DOWN: u32 = 1 << 1;

/// A login the user just submitted, waiting for Save / Never / Not now.
struct PendingLogin {
    tab: u64,
    origin: String,
    username: String,
    password: String,
}

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
    /// 🔊 while the tab plays sound, 🔇 while it's muted (click to toggle).
    sound: LabelButton,
    close: LabelButton,
    /// Set while the tab shows a built-in page (bare://downloads, bare://settings).
    page: Option<Page>,
    /// Permission requests waiting for an answer, oldest first.
    prompts: VecDeque<PermissionRequest>,
    /// The page's process and its private memory (bytes), sampled every few seconds.
    pid: Option<u32>,
    memory: Option<u64>,
    /// Last address written to history from this tab (so a re-reported address isn't a new visit).
    recorded: String,
    /// Reader mode: the page looks like an article; the reader page this tab made (its data:
    /// URL and the page it came from); whether that is what it shows now; and the token of the
    /// article on its way (only an article carrying it is accepted).
    readerable: bool,
    reader: Option<(String, String)>,
    in_reader: bool,
    reader_pending: Option<String>,
    /// Frames of the page currently playing sound, and whether the tab is muted.
    audible: HashSet<String>,
    muted: bool,
}

impl Tab {
    fn browser_id(&self) -> Option<i32> {
        self.browser.as_ref().map(|b| b.identifier())
    }

    fn title_text(&self) -> &str {
        match (self.title.trim(), self.url.as_str()) {
            ("", "") | ("", url::BLANK) | ("about:blank", _) => "New tab",
            ("", u) => u,
            (t, _) => t,
        }
    }

    /// The tab's title cut to `max_chars`, with a dot while loading.
    fn label(&self, max_chars: usize) -> String {
        let title = self.title_text();
        let max_chars = max_chars.max(1);
        let mut label: String = title.chars().take(max_chars).collect();
        if title.chars().count() > max_chars {
            label.push('…');
        }
        if self.loading {
            label.insert_str(0, "• ");
        }
        label
    }
}

/// Launch target meaning "a new tab" (whatever the new-tab setting says).
pub const NEW_TAB: &str = "bare:new-tab";

/// Rows in the address-bar suggestions.
const MAX_SUGGESTIONS: usize = 8;

/// Solid colours for the suggestions (they float over the page, so they can't be see-through).
fn suggest_background() -> u32 {
    if crate::system::dark_mode() { 0xFF2B2B30 } else { 0xFFFFFFFF }
}

fn suggest_highlight() -> u32 {
    if crate::system::dark_mode() { 0xFF45454D } else { 0xFFE3E8F0 }
}

const TAB_MIN_WIDTH: i32 = 72;
const TAB_MAX_WIDTH: i32 = 220;
/// Below this width, inactive tabs hide their ✕ (like Chrome).
const TAB_CLOSE_MIN_WIDTH: i32 = 120;
const TAB_CLOSE_WIDTH: i32 = 26;
const TAB_CHAR_WIDTH: i32 = 7;

/// How the tab strip fits `n` tabs into `avail` pixels: the width of each tab, the first
/// visible tab and how many are visible. The active tab is always visible; the rest scroll.
fn tab_layout(n: usize, active: usize, first: usize, avail: i32) -> (i32, usize, usize) {
    if n == 0 {
        return (TAB_MAX_WIDTH, 0, 0);
    }
    let width = (avail / n as i32).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH);
    let fit = ((avail / width).max(1) as usize).min(n);
    let mut first = first.min(n - fit);
    if active < first {
        first = active;
    } else if active >= first + fit {
        first = active + 1 - fit;
    }
    (width, first, fit)
}

pub struct Ui {
    window: Window,
    strip: Panel,
    /// "▾ N": tabs that don't fit, as a menu.
    overflow: LabelButton,
    /// First tab shown in the strip when not all fit.
    first_tab: usize,
    back: LabelButton,
    forward: LabelButton,
    address: Textfield,
    shield: LabelButton,
    /// "125%": shown when the page isn't at 100%; clicking resets.
    zoom: LabelButton,
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
    /// ☆ / ★: bookmark the current page.
    star: LabelButton,
    /// 📖: open or leave reader view (shown on pages that look like articles).
    reader: LabelButton,
    /// 🔑: fill a saved login (shown on sites with saved logins).
    key: LabelButton,
    /// Usernames behind the open 🔑 menu (command LOGIN_MENU_BASE + index).
    menu_logins: Vec<String>,
    /// "Save password for … ?" bar, and the login it is about.
    save_bar: Panel,
    save_text: LabelButton,
    pending_login: Option<PendingLogin>,
    /// Saved passwords can be shown without asking again until this time (after Windows
    /// Hello), and whether a Windows Hello prompt is open.
    reveal_until: Option<std::time::Instant>,
    verifying: bool,
    /// The password export (CSV) imported last, which the passwords page offers to delete.
    imported_csv: Option<std::path::PathBuf>,
    /// The bookmarks bar, its link addresses and folder buttons/contents.
    bookmark_bar: Panel,
    bar_urls: Vec<String>,
    bar_folders: Vec<(LabelButton, Vec<bookmarks::Bookmark>)>,
    /// Every bar entry in order, the "»" button, and the first entry that didn't fit.
    bar_entries: Vec<(LabelButton, bookmarks::BarItem)>,
    bar_more: LabelButton,
    bar_hidden_from: usize,
    /// Addresses behind the open folder menu (command BOOKMARK_MENU_BASE + index).
    menu_urls: Vec<String>,
    /// Address-bar suggestions: a floating panel of rows, their targets, the highlighted one.
    suggest_overlay: Option<OverlayController>,
    suggest_rows: Vec<LabelButton>,
    suggestions: Vec<String>,
    suggest_index: Option<usize>,
    last_typed: String,
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
        let column = panel.set_to_box_layout(Some(&BoxLayoutSettings {
            horizontal: 0,
            // Every row (tab strip, toolbar, bars) spans the full width.
            cross_axis_alignment: AxisAlignment::STRETCH,
            ..Default::default()
        }));

        let (strip, _) = row(2);
        let overflow = button("▾", Action::TabMenu, "All tabs");
        View::from(&overflow).set_visible(0);
        add(&strip, &overflow);
        add(&strip, &button("+", Action::NewTab, "New tab (Ctrl+T)"));

        let (nav, nav_layout) = row(4);
        let back = button("←", Action::Back, "Back (Alt+Left)");
        let forward = button("→", Action::Forward, "Forward (Alt+Right)");
        let reload = button("↻", Action::Reload, "Reload (F5)");
        let home = button("🏠", Action::Home, "Home: your search engine (Alt+Home)");
        let address = textfield_create(Some(&mut Address::new())).expect("textfield");
        let star = button("☆", Action::ToggleBookmark, "Bookmark this page (Ctrl+D)");
        let reader = button("📖", Action::Reader, "Reader view (F9)");
        View::from(&reader).set_visible(0);
        let key = button("🔑", Action::Logins, "Fill a saved password");
        View::from(&key).set_visible(0);
        address.set_placeholder_text(Some(&CefString::from("Search or enter address")));
        let zoom = button("100%", Action::ZoomReset, "Reset zoom (Ctrl+0)");
        View::from(&zoom).set_visible(0);
        let shield = button("", Action::Shield, "");
        let update = button("Update ready", Action::UpdateRestart, "Restart Bare to install the update");
        let downloads = button("↓", Action::Downloads, "Downloads (Ctrl+J)");
        let settings = button("⚙", Action::Settings, "Settings");
        View::from(&update).set_visible(0);
        for v in [
            View::from(&back),
            View::from(&forward),
            View::from(&reload),
            View::from(&home),
            View::from(&address),
            View::from(&key),
            View::from(&reader),
            View::from(&star),
            View::from(&zoom),
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

        // Save-password bar: "Save password for me@example.com on example.com?" [Save] [Never] [Not now]
        let (save_bar, save_layout) = row(8);
        let save_text = label("");
        add(&save_bar, &save_text);
        add(&save_bar, &button("Save", Action::SaveLogin, "Save in Bare's password vault (encrypted for your Windows account)"));
        add(&save_bar, &button("Never for this site", Action::NeverSaveLogin, "Don't offer to save passwords on this site"));
        add(&save_bar, &button("Not now", Action::DismissLogin, ""));
        if let Some(l) = save_layout {
            l.set_flex_for_view(Some(&mut View::from(&save_text)), 1);
        }
        View::from(&save_text).set_background_color(0);
        save_text.set_horizontal_alignment(HorizontalAlignment::LEFT);
        View::from(&save_bar).set_background_color(0x402563EB);
        View::from(&save_bar).set_visible(0);

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
        // Bookmarks bar (filled by rebuild_bookmarks_bar).
        let (bookmark_bar, _) = row(2);
        add(&panel, &bookmark_bar);
        add(&panel, &prompt_bar);
        add(&panel, &save_bar);
        add(&panel, &find_bar);

        for (i, &(_, key, shift, ctrl, alt)) in SHORTCUTS.iter().enumerate() {
            window.set_accelerator(SHORTCUT_BASE + i as c_int, key, shift as _, ctrl as _, alt as _, 1);
        }

        // Address-bar suggestions: a column of rows floating over the page, under the address bar.
        let suggest_panel = panel_create(None).expect("panel");
        suggest_panel.set_to_box_layout(Some(&BoxLayoutSettings {
            horizontal: 0,
            inside_border_vertical_spacing: 4,
            inside_border_horizontal_spacing: 4,
            cross_axis_alignment: AxisAlignment::STRETCH,
            ..Default::default()
        }));
        let suggest_rows: Vec<LabelButton> = (0..MAX_SUGGESTIONS)
            .map(|i| {
                let b = button("", Action::PickSuggestion(i), "");
                b.set_horizontal_alignment(HorizontalAlignment::LEFT);
                add(&suggest_panel, &b);
                b
            })
            .collect();
        View::from(&suggest_panel).set_background_color(suggest_background());
        for row in &suggest_rows {
            View::from(row).set_background_color(suggest_background());
        }
        let suggest_overlay = window.add_overlay_view(Some(&mut View::from(&suggest_panel)), DockingMode::CUSTOM, 0);
        if let Some(o) = &suggest_overlay {
            o.set_visible(0);
        }

        Ui {
            window: window.clone(),
            strip,
            overflow,
            first_tab: 0,
            back,
            forward,
            address,
            shield,
            zoom,
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
            star,
            reader,
            key,
            menu_logins: Vec::new(),
            save_bar,
            save_text,
            pending_login: None,
            reveal_until: None,
            verifying: false,
            imported_csv: None,
            bookmark_bar,
            bar_urls: Vec::new(),
            bar_folders: Vec::new(),
            bar_entries: Vec::new(),
            bar_more: button("»", Action::BookmarksMore, "More bookmarks"),
            bar_hidden_from: usize::MAX,
            menu_urls: Vec::new(),
            suggest_overlay,
            suggest_rows,
            suggestions: Vec::new(),
            suggest_index: None,
            last_typed: String::new(),
        }
    }

    pub fn open(&mut self, target: &str) {
        let target = if target.is_empty() { url::BLANK } else { target };
        self.open_tab(target, target == url::BLANK);
    }

    /// Opens `target` in a new tab, focusing the address bar (ready to type) or the page.
    fn open_tab(&mut self, target: &str, focus_address: bool) {
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
        let sound = button("🔊", Action::ToggleMute(id), "Mute tab");
        View::from(&sound).set_visible(0);
        let close = button("✕", Action::Close(id), "Close tab (Ctrl+W)");
        // Tab k's label, 🔊 and ✕ sit at strip positions 3k, 3k+1 and 3k+2, before "▾" and "+".
        let at = 3 * self.tabs.len() as c_int;
        self.strip.add_child_view_at(Some(&mut View::from(&label)), at);
        self.strip.add_child_view_at(Some(&mut View::from(&sound)), at + 1);
        self.strip.add_child_view_at(Some(&mut View::from(&close)), at + 2);

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
            sound,
            close,
            page: None,
            prompts: VecDeque::new(),
            pid: None,
            memory: None,
            recorded: String::new(),
            readerable: false,
            reader: None,
            in_reader: false,
            reader_pending: None,
            audible: HashSet::new(),
            muted: false,
        });
        self.active = self.tabs.len() - 1;
        Panel::from(&self.window).layout();
        self.refresh();
        if focus_address { self.focus_address() } else { self.focus_page() }
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
        if self.pending_login.as_ref().is_some_and(|p| p.tab == tab.id) {
            self.pending_login = None;
        }
        for request in tab.prompts.drain(..) {
            request.answer(false);
        }
        self.strip.remove_child_view(Some(&mut View::from(&tab.button)));
        self.strip.remove_child_view(Some(&mut View::from(&tab.sound)));
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
            Action::NewTab => self.open_new_tab(),
            Action::CloseActive => self.close(self.active),
            Action::NextTab if n > 0 => self.select((self.active + 1) % n),
            Action::PrevTab if n > 0 => self.select((self.active + n - 1) % n),
            Action::FocusAddress => self.focus_address(),
            Action::Home => {
                let home = settings::get().search.home();
                self.load_in_active(home);
            }
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
            Action::Reader => self.toggle_reader(),
            Action::Logins => self.show_logins(),
            Action::SaveLogin | Action::NeverSaveLogin | Action::DismissLogin => {
                if let Some(p) = self.pending_login.take() {
                    match action {
                        Action::SaveLogin => passwords::save(&p.origin, &p.username, &p.password),
                        Action::NeverSaveLogin => passwords::set_never(&p.origin, true),
                        _ => {}
                    }
                    self.refresh();
                    self.refresh_pages(Page::Passwords);
                }
            }
            Action::Downloads => self.open_page(Page::Downloads),
            Action::Settings => self.open_page(Page::Settings),
            Action::UpdateRestart => {
                update::request_restart();
                self.close_window();
            }
            Action::PermissionAllow => self.answer_permission(true),
            Action::PermissionBlock => self.answer_permission(false),
            Action::TabMenu => self.show_tab_menu(),
            Action::HistoryPage => self.open_page(Page::History),
            Action::BookmarksPage => self.open_page(Page::Bookmarks),
            Action::ToggleBookmark => self.toggle_bookmark(),
            Action::ToggleBookmarksBar => {
                settings::update(|s| s.bookmarks_bar = !s.bookmarks_bar);
                self.rebuild_bookmarks_bar();
                self.refresh_pages(Page::Bookmarks);
            }
            Action::OpenBookmark(i) => {
                if let Some(url) = self.bar_urls.get(i).cloned() {
                    self.load_in_active(&url);
                }
            }
            Action::BookmarkFolder(i) => self.show_folder_menu(i),
            Action::BookmarksMore => self.show_bookmarks_more(),
            Action::PickSuggestion(i) => {
                if let Some(url) = self.suggestions.get(i).cloned() {
                    self.load_in_active(&url);
                }
                self.hide_suggestions();
            }
            Action::SelectTab(i) => self.select(i),
            Action::LastTab if n > 0 => self.select(n - 1),
            Action::MoveTabLeft => self.move_tab(false),
            Action::MoveTabRight => self.move_tab(true),
            Action::PictureInPicture => {
                if let Some(frame) = self.tabs.get(self.active).and_then(|t| t.browser.as_ref()).and_then(|b| b.main_frame()) {
                    let script = handlers::pip_script(None);
                    frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
                }
            }
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                let Some(host) = self.active_host() else { return };
                host.zoom(match action {
                    Action::ZoomIn => ZoomCommand::IN,
                    Action::ZoomOut => ZoomCommand::OUT,
                    _ => ZoomCommand::RESET,
                });
                // The new level is applied asynchronously.
                defer_after(150, |ui| ui.refresh());
            }
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
            Action::ToggleMute(id) => self.toggle_mute(id),
            _ => {}
        }
    }

    // ---- sound ----

    /// A frame of a tab started or stopped playing sound.
    pub fn on_audio(&mut self, browser_id: i32, frame: String, playing: bool) {
        let Some(t) = self.tab_for_browser(browser_id) else { return };
        let was = !t.audible.is_empty();
        if !playing {
            t.audible.remove(&frame);
        } else if t.audible.len() < 64 {
            t.audible.insert(frame);
        }
        if was != !t.audible.is_empty() {
            self.layout_tabs();
        }
    }

    /// The tab started loading a new page: nothing from the old one is playing any more.
    pub fn on_audio_reset(&mut self, browser_id: i32) {
        let changed = self.tab_for_browser(browser_id).is_some_and(|t| {
            let was = !t.audible.is_empty();
            t.audible.clear();
            was
        });
        if changed {
            self.layout_tabs();
        }
    }

    /// 🔊 / 🔇: mutes or unmutes a tab (it stays muted on other pages until unmuted).
    fn toggle_mute(&mut self, id: u64) {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else { return };
        t.muted = !t.muted;
        if let Some(host) = t.browser.as_ref().and_then(|b| b.host()) {
            host.set_audio_muted(t.muted as c_int);
        }
        self.layout_tabs();
    }

    /// Opens reader view for the active tab, or leaves it (back to the original page).
    fn toggle_reader(&mut self) {
        let Some(t) = self.tabs.get_mut(self.active) else { return };
        let Some(b) = t.browser.clone() else { return };
        if t.in_reader {
            if t.can_back {
                b.go_back();
            } else if let Some((_, source)) = &t.reader {
                b.main_frame().map(|f| f.load_url(Some(&CefString::from(source.as_str()))));
            }
        } else if t.readerable && t.page.is_none() {
            let token = pages::random_token();
            if let Some(f) = b.main_frame() {
                f.execute_java_script(Some(&CefString::from(reader::extract_script(&token).as_str())), None, 0);
            }
            t.reader_pending = Some(token);
        }
    }

    /// A reader script reported back from a page.
    pub fn on_reader_report(&mut self, browser_id: i32, report: reader::Report) {
        let Some(t) = self.tab_for_browser(browser_id) else { return };
        match report {
            reader::Report::Readerable(yes) => t.readerable = yes && !t.in_reader,
            // Only the article the user asked for, so a page can't open reader view by itself.
            reader::Report::Article(token, article) if t.reader_pending.as_ref() == Some(&token) => {
                t.reader_pending = None;
                let Some(article) = article else {
                    t.readerable = false; // Readability found nothing after all
                    return self.refresh();
                };
                let page = reader::page_url(&article, &t.url);
                if let Some(f) = t.browser.as_ref().and_then(|b| b.main_frame()) {
                    f.load_url(Some(&CefString::from(page.as_str())));
                }
                t.reader = Some((page, t.url.clone()));
            }
            reader::Report::Article(..) => {}
        }
        self.refresh();
    }

    // ---- passwords ----

    /// The active tab's site, if it's a web page that could have saved logins.
    fn active_origin(&self) -> Option<String> {
        self.tabs.get(self.active).filter(|t| t.page.is_none() && !t.in_reader).and_then(|t| passwords::origin(&t.url))
    }

    /// A page reported a submitted login: offer to save it.
    pub fn on_login_submitted(&mut self, browser_id: i32, login: passwords::Submitted) {
        if self.private || !settings::get().save_passwords {
            return;
        }
        let Some(t) = self.tabs.iter().find(|t| t.browser_id() == Some(browser_id)) else { return };
        // The page says which site it is; believe it only if that's where the tab really is.
        if t.page.is_some() || passwords::origin(&t.url).as_deref() != Some(login.origin.as_str()) {
            return;
        }
        if passwords::offer(&login.origin, &login.username, &login.password) == passwords::Offer::None {
            return;
        }
        self.pending_login = Some(PendingLogin { tab: t.id, origin: login.origin, username: login.username, password: login.password });
        self.refresh();
    }

    fn refresh_save_bar(&self) {
        let active = self.tabs.get(self.active).map(|t| t.id);
        let pending = self.pending_login.as_ref().filter(|p| Some(p.tab) == active);
        View::from(&self.save_bar).set_visible(pending.is_some() as _);
        if let Some(p) = pending {
            let verb = match passwords::offer(&p.origin, &p.username, &p.password) {
                passwords::Offer::Update => "Update",
                _ => "Save",
            };
            let who = if p.username.is_empty() { String::new() } else { format!(" for {}", p.username) };
            let text = format!("{verb} password{who} on {}?", passwords::site(&p.origin));
            self.save_text.set_text(Some(&CefString::from(text.as_str())));
        }
    }

    /// 🔑: fill the only saved login, or choose one from a menu.
    fn show_logins(&mut self) {
        let Some(origin) = self.active_origin() else { return };
        self.menu_logins = passwords::usernames(&origin);
        match self.menu_logins.len() {
            0 => {}
            1 => self.fill_login(0),
            _ => {
                let Some(mut menu) = menu_model_create(Some(&mut LoginMenu::new())) else { return };
                for (i, name) in self.menu_logins.iter().enumerate() {
                    let label = if name.is_empty() { "(no username)" } else { name.as_str() };
                    menu.add_item(LOGIN_MENU_BASE + i as c_int, Some(&CefString::from(label.chars().take(80).collect::<String>().as_str())));
                }
                let b = View::from(&self.key).bounds_in_screen();
                self.window.show_menu(Some(&mut menu), Some(&Point { x: b.x, y: b.y + b.height }), MenuAnchorPosition::TOPLEFT);
            }
        }
    }

    /// "Show" on bare://passwords: asks Windows Hello first (then not again for a minute).
    fn reveal_password(&mut self, browser_id: i32, origin: String, username: String) {
        if self.reveal_until.is_some_and(|t| std::time::Instant::now() < t) {
            return self.show_password(browser_id, &origin, &username);
        }
        if self.verifying {
            return;
        }
        self.verifying = true;
        let window = self.window.window_handle().0 as isize;
        background(move || {
            let outcome = crate::hello::verify(window, "Bare wants to show a saved password.");
            Box::new(move |ui: &mut Ui| ui.on_verified(outcome, browser_id, &origin, &username))
        });
    }

    fn on_verified(&mut self, outcome: crate::hello::Outcome, browser_id: i32, origin: &str, username: &str) {
        use crate::hello::Outcome;
        self.verifying = false;
        match outcome {
            Outcome::Verified => {
                self.reveal_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
                self.show_password(browser_id, origin, username);
            }
            // No Windows Hello on this PC: nothing to ask with.
            Outcome::Unavailable => self.show_password(browser_id, origin, username),
            Outcome::Refused => {}
        }
    }

    /// Sends one password to the passwords page that asked for it (and only to that page).
    fn show_password(&self, browser_id: i32, origin: &str, username: &str) {
        let Some(password) = passwords::password(origin, username) else { return };
        let tab = self.tabs.iter().find(|t| t.browser_id() == Some(browser_id) && t.page == Some(Page::Passwords));
        if let Some(f) = tab.and_then(|t| t.browser.as_ref()).and_then(|b| b.main_frame()) {
            let js = crate::block::js_string;
            let script = format!("window.reveal&&reveal({},{},{})", js(origin), js(username), js(&password));
            f.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
        }
    }

    /// "Import passwords": a file picker for a CSV export from another browser.
    fn choose_password_csv(&self, browser_id: i32) {
        let tab = self.tabs.iter().find(|t| t.browser_id() == Some(browser_id));
        let Some(host) = tab.and_then(|t| t.browser.as_ref()).and_then(|b| b.host()) else { return };
        let mut filters = CefStringList::new();
        filters.append(".csv");
        host.run_file_dialog(
            FileDialogMode::OPEN,
            Some(&CefString::from("Import passwords (CSV)")),
            None,
            Some(&mut filters),
            Some(&mut CsvChosen::new()),
        );
    }

    pub fn import_password_csv(&mut self, file: std::path::PathBuf) {
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let text = match std::fs::metadata(&file) {
            Ok(m) if m.len() > 50 * 1024 * 1024 => Err("the file is too large".to_string()),
            _ => std::fs::read(&file).map(|b| String::from_utf8_lossy(&b).into_owned()).map_err(|e| e.to_string()),
        };
        match text.and_then(|t| passwords::import_csv(&t)) {
            Ok(r) => {
                let skipped = if r.skipped > 0 { format!(" ({} skipped: already saved, or not websites)", r.skipped) } else { String::new() };
                let s = if r.added == 1 { "" } else { "s" };
                pages::set_password_notice(
                    &format!("Imported {} password{s}{skipped}. {name} holds your passwords unencrypted: delete it now.", r.added),
                    Some(&name),
                );
                self.imported_csv = Some(file);
            }
            Err(e) => pages::set_password_notice(&format!("Couldn\u{2019}t import {name}: {e}."), None),
        }
        self.refresh_pages(Page::Passwords);
        self.refresh();
    }

    /// Fills the login picked from the 🔑 menu into the active page.
    pub fn fill_login(&mut self, index: usize) {
        let (Some(origin), Some(username)) = (self.active_origin(), self.menu_logins.get(index).cloned()) else { return };
        let Some(password) = passwords::password(&origin, &username) else { return };
        let frame = self.tabs.get(self.active).and_then(|t| t.browser.as_ref()).and_then(|b| b.main_frame());
        if let Some(f) = frame {
            f.execute_java_script(Some(&CefString::from(passwords::fill_script(&origin, &username, &password).as_str())), None, 0);
        }
        self.focus_page();
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

    /// A new tab: the search engine's home page (or blank), with the address bar ready to type in.
    pub fn open_new_tab(&mut self) {
        let url = settings::get().new_tab_url();
        self.open_tab(url, true);
    }

    /// Opens a launch target in a new tab: a web address, a file, or a bare:// page.
    pub fn open_target(&mut self, target: &str) {
        if target == NEW_TAB {
            return self.open_new_tab();
        }
        match Page::from_address(target) {
            Some(page) => self.open_page(page),
            None => self.open(target),
        }
    }

    // ---- address-bar suggestions ----

    pub fn suggestions_visible(&self) -> bool {
        self.suggest_overlay.as_ref().is_some_and(|o| o.is_visible() == 1)
    }

    pub fn hide_suggestions(&mut self) {
        if let Some(o) = &self.suggest_overlay {
            o.set_visible(0);
        }
        self.suggest_index = None;
        self.last_typed.clear();
    }

    /// Rebuilds the suggestions from what's typed: what Enter would do, then matching
    /// bookmarks (★) and history, best first.
    pub fn update_suggestions(&mut self) {
        let typed = CefString::from(&self.address.text()).to_string();
        if typed == self.last_typed {
            return; // cursor moved, nothing typed
        }
        if typed.trim().is_empty() || Page::from_address(&typed).is_some() {
            return self.hide_suggestions();
        }
        self.last_typed = typed.clone();
        let search = settings::get().search;
        let target = url::resolve(&typed, search.prefix());
        let first = if target.starts_with(search.prefix()) {
            format!("🔍  {}  —  {} search", typed.trim(), search.name())
        } else {
            format!("→  {}", history::display_url(&target))
        };
        let mut rows: Vec<(String, String)> = vec![(target, first)];
        for b in bookmarks::suggest(&typed, MAX_SUGGESTIONS) {
            rows.push((b.url.clone(), format!("★  {}  —  {}", b.title, history::display_url(&b.url))));
        }
        if !self.private {
            for e in history::suggest(&typed, MAX_SUGGESTIONS) {
                let title = if e.title.is_empty() { history::display_url(&e.url).to_string() } else { e.title.clone() };
                rows.push((e.url.clone(), format!("{title}  —  {}", history::display_url(&e.url))));
            }
        }
        let mut seen = HashSet::new();
        rows.retain(|(u, _)| seen.insert(u.trim_end_matches('/').to_string()));
        rows.truncate(MAX_SUGGESTIONS);
        self.suggestions = rows.iter().map(|(u, _)| u.clone()).collect();
        self.suggest_index = None;
        for (i, row) in self.suggest_rows.iter().enumerate() {
            let label = rows.get(i).map_or("", |(_, l)| l.as_str());
            let label: String = label.chars().take(120).collect();
            row.set_text(Some(&CefString::from(label.as_str())));
            View::from(row).set_visible((i < rows.len()) as _);
            View::from(row).set_background_color(suggest_background());
        }
        self.place_suggestions(rows.len());
    }

    /// Positions the suggestions right under the address bar, as wide as it.
    fn place_suggestions(&self, count: usize) {
        let Some(o) = &self.suggest_overlay else { return };
        // Overlay positions are relative to the window's client area.
        let field = View::from(&self.address).bounds_in_screen();
        let client = self.window.client_area_bounds_in_screen();
        o.set_bounds(Some(&Rect {
            x: field.x - client.x,
            y: field.y - client.y + field.height + 2,
            width: field.width,
            height: count as c_int * 30 + 8,
        }));
        o.set_visible(1);
    }

    pub fn move_suggestion(&mut self, down: bool) {
        if !self.suggestions_visible() || self.suggestions.is_empty() {
            return;
        }
        let n = self.suggestions.len();
        let next = match (self.suggest_index, down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        self.suggest_index = Some(next);
        for (i, row) in self.suggest_rows.iter().enumerate() {
            View::from(row).set_background_color(if i == next { suggest_highlight() } else { suggest_background() });
        }
    }

    /// Enter in the address bar: the highlighted suggestion if any, else what's typed.
    pub fn enter_address(&mut self, input: &str) {
        match self.suggest_index.and_then(|i| self.suggestions.get(i).cloned()) {
            Some(url) => self.load_in_active(&url),
            None => self.navigate(input),
        }
        self.hide_suggestions();
    }

    fn load_in_active(&mut self, target: &str) {
        if let Some(page) = Page::from_address(target) {
            return self.open_page(page);
        }
        if let Some(frame) = self.tabs.get(self.active).and_then(|t| t.browser.as_ref()).and_then(|b| b.main_frame()) {
            frame.load_url(Some(&CefString::from(target)));
        }
        self.focus_page();
    }

    // ---- bookmarks ----

    /// Refills the bookmarks bar: loose bookmarks as buttons, folders as menus.
    fn rebuild_bookmarks_bar(&mut self) {
        self.bookmark_bar.remove_all_child_views();
        self.bar_urls.clear();
        self.bar_folders.clear();
        self.bar_entries.clear();
        let items = bookmarks::bar_items();
        for item in &items {
            match item {
                bookmarks::BarItem::Link(b) => {
                    let title = if b.title.is_empty() { history::display_url(&b.url) } else { &b.title };
                    let label: String = title.chars().take(24).collect();
                    let button = button(&label, Action::OpenBookmark(self.bar_urls.len()), &b.url);
                    add(&self.bookmark_bar, &button);
                    self.bar_urls.push(b.url.clone());
                    self.bar_entries.push((button, item.clone()));
                }
                bookmarks::BarItem::Folder(name, list) => {
                    let label: String = name.chars().take(24).collect();
                    let button = button(&format!("{label} ▾"), Action::BookmarkFolder(self.bar_folders.len()), name);
                    add(&self.bookmark_bar, &button);
                    self.bar_folders.push((button.clone(), list.clone()));
                    self.bar_entries.push((button, item.clone()));
                }
            }
        }
        add(&self.bookmark_bar, &self.bar_more);
        let show = settings::get().bookmarks_bar && !items.is_empty();
        View::from(&self.bookmark_bar).set_visible(show as _);
        self.fit_bookmarks_bar();
    }

    /// Shows as many bar entries as fit the window; the rest go behind "»".
    fn fit_bookmarks_bar(&mut self) {
        let avail = View::from(&Panel::from(&self.window)).bounds().width;
        let avail = if avail > 0 { avail } else { 1280 } - 60; // room for "»" and padding
        let mut used = 0;
        self.bar_hidden_from = usize::MAX;
        for (i, (button, _)) in self.bar_entries.iter().enumerate() {
            let view = View::from(button);
            used += view.preferred_size().width + 2;
            let fits = used <= avail && self.bar_hidden_from == usize::MAX;
            if !fits && self.bar_hidden_from == usize::MAX {
                self.bar_hidden_from = i;
            }
            view.set_visible(fits as _);
        }
        View::from(&self.bar_more).set_visible((self.bar_hidden_from != usize::MAX) as _);
        Panel::from(&self.window).layout();
    }

    /// The "»" menu: bar entries that didn't fit, folders as sub-menus.
    fn show_bookmarks_more(&mut self) {
        let Some(menu) = menu_model_create(Some(&mut BookmarkMenu::new())) else { return };
        self.menu_urls.clear();
        let hidden: Vec<bookmarks::BarItem> = self.bar_entries.iter().skip(self.bar_hidden_from).map(|(_, item)| item.clone()).collect();
        let add_link = |menu: &MenuModel, urls: &mut Vec<String>, b: &bookmarks::Bookmark| {
            let title = if b.title.is_empty() { history::display_url(&b.url).to_string() } else { b.title.clone() };
            menu.add_item(BOOKMARK_MENU_BASE + urls.len() as c_int, Some(&CefString::from(title.chars().take(80).collect::<String>().as_str())));
            urls.push(b.url.clone());
        };
        let mut urls = Vec::new();
        for (i, item) in hidden.iter().enumerate() {
            match item {
                bookmarks::BarItem::Link(b) => add_link(&menu, &mut urls, b),
                bookmarks::BarItem::Folder(name, list) => {
                    if let Some(sub) = menu.add_sub_menu(BOOKMARK_SUBMENU_BASE + i as c_int, Some(&CefString::from(name.as_str()))) {
                        for b in list {
                            add_link(&sub, &mut urls, b);
                        }
                    }
                }
            }
        }
        self.menu_urls = urls;
        let b = View::from(&self.bar_more).bounds_in_screen();
        let mut menu = menu;
        self.window.show_menu(Some(&mut menu), Some(&Point { x: b.x, y: b.y + b.height }), MenuAnchorPosition::TOPRIGHT);
    }

    fn toggle_bookmark(&mut self) {
        let Some(t) = self.tabs.get(self.active) else { return };
        if t.page.is_some() || !history::recordable(&t.url) {
            return;
        }
        if bookmarks::is_bookmarked(&t.url) {
            bookmarks::remove(&t.url);
        } else {
            bookmarks::add(&t.title, &t.url);
        }
        self.rebuild_bookmarks_bar();
        self.refresh_pages(Page::Bookmarks);
        self.refresh();
    }

    fn show_folder_menu(&mut self, index: usize) {
        let Some((button, list)) = self.bar_folders.get(index) else { return };
        let Some(menu) = menu_model_create(Some(&mut BookmarkMenu::new())) else { return };
        self.menu_urls = list.iter().map(|b| b.url.clone()).collect();
        for (i, b) in list.iter().enumerate() {
            // Sub-folders show their path, e.g. "Docs › Page".
            let sub = b.folder.split_once('/').map(|(_, rest)| rest.replace('/', " › "));
            let title = if b.title.is_empty() { history::display_url(&b.url).to_string() } else { b.title.clone() };
            let label = match sub {
                Some(path) => format!("{path} › {title}"),
                None => title,
            };
            menu.add_item(BOOKMARK_MENU_BASE + i as c_int, Some(&CefString::from(label.chars().take(80).collect::<String>().as_str())));
        }
        let b = View::from(button).bounds_in_screen();
        let mut menu = menu;
        self.window.show_menu(Some(&mut menu), Some(&Point { x: b.x, y: b.y + b.height }), MenuAnchorPosition::TOPLEFT);
    }

    pub fn open_menu_url(&mut self, index: usize) {
        if let Some(url) = self.menu_urls.get(index).cloned() {
            self.load_in_active(&url);
        }
    }

    /// Shows the window in front of other apps (restoring it if minimised).
    pub fn bring_to_front(&self) {
        if self.window.is_minimized() == 1 {
            self.window.restore();
        }
        self.window.show();
        self.window.activate();
        self.window.bring_to_top();
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
                    "newTabHome" => s.new_tab_home = on,
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
            (Some(Page::History), "hist-delete") => history::delete(param("url")),
            (Some(Page::History), "hist-clear") => history::clear(),
            (Some(Page::History), "hist-search") => pages::set_history_query(param("q")),
            (Some(Page::Bookmarks), "bm-delete") => {
                bookmarks::remove(param("url"));
                self.rebuild_bookmarks_bar();
            }
            (Some(Page::Bookmarks), "bm-rename") => {
                bookmarks::rename(param("url"), param("title"));
                self.rebuild_bookmarks_bar();
            }
            (Some(Page::Bookmarks), "bm-import") => {
                let from = param("from");
                let name = bookmarks::import_sources().into_iter().find(|(id, ..)| *id == from).map_or("that browser", |(_, n, _)| n);
                match bookmarks::import(from) {
                    Ok(0) => pages::set_notice(&format!("Nothing new to import from {name}.")),
                    Ok(n) => pages::set_notice(&format!("Imported {n} bookmark{} from {name}.", if n == 1 { "" } else { "s" })),
                    Err(e) => pages::set_notice(&format!("Import failed: {e}")),
                }
                self.rebuild_bookmarks_bar();
            }
            (Some(Page::Bookmarks), "set") if param("key") == "bookmarksBar" => {
                let on = param("value") == "true";
                settings::update(|s| s.bookmarks_bar = on);
                self.rebuild_bookmarks_bar();
            }
            (Some(Page::Settings), "allow-remove") => {
                let host = param("host").to_string();
                settings::update(|s| {
                    s.allowed_sites.remove(&host);
                });
            }
            (Some(Page::Settings), "clear-data") => self.clear_cookies_and_cache(),
            (Some(Page::Settings), "update-check") => update::check(),
            (Some(Page::Settings), "make-default") => crate::system::open_default_apps_settings(),
            (Some(Page::Settings), "update-restart") => self.act(Action::UpdateRestart),
            (Some(Page::Settings), "open-passwords") => return self.open_page(Page::Passwords),
            (Some(Page::Settings), "whats-new") => {
                let notes = format!("{}/releases/tag/v{}", env!("CARGO_PKG_REPOSITORY"), update::VERSION);
                return self.open(&notes);
            }
            (Some(Page::Settings), "engine-check") => crate::engine::check(),
            (Some(Page::Settings), "engine-update") => crate::engine::download(),
            (Some(Page::Passwords), "pw-reveal") => {
                return self.reveal_password(browser_id, param("origin").into(), param("username").into());
            }
            (Some(Page::Passwords), "pw-import") => return self.choose_password_csv(browser_id),
            (Some(Page::Passwords), "pw-delete-csv") => {
                if let Some(file) = self.imported_csv.take() {
                    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    match std::fs::remove_file(&file) {
                        Ok(()) => pages::set_password_notice(&format!("Deleted {name}."), None),
                        Err(e) => pages::set_password_notice(&format!("Couldn\u{2019}t delete {name}: {e}"), None),
                    }
                }
            }
            (Some(Page::Passwords), "pw-delete") => passwords::remove(param("origin"), param("username")),
            (Some(Page::Passwords), "pw-never-remove") => passwords::set_never(param("origin"), false),
            (Some(Page::Passwords), "pw-offer") => {
                let on = param("value") == "true";
                settings::update(|s| s.save_passwords = on);
            }
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
        self.hide_suggestions();
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

    /// Sizes the tabs to fit the strip: they shrink as more open; when even the narrowest
    /// don't fit, the rest go behind the "▾ N" menu button. The active tab is always shown.
    fn layout_tabs(&mut self) {
        // The window's width (the strip itself only spans its contents).
        let strip_width = View::from(&Panel::from(&self.window)).bounds().width;
        let strip_width = if strip_width > 0 { strip_width } else { 1280 };
        // Room for the "+" and "▾ N" buttons and padding.
        let avail = strip_width - 120;
        let (width, first, fit) = tab_layout(self.tabs.len(), self.active, self.first_tab, avail);
        self.first_tab = first;
        for (i, t) in self.tabs.iter().enumerate() {
            let visible = i >= first && i < first + fit;
            let active = i == self.active;
            let show_close = visible && (active || width >= TAB_CLOSE_MIN_WIDTH);
            // 🔊 / 🔇 while the tab plays sound or is muted (on visible tabs wide enough for it).
            let show_sound = visible && (t.muted || !t.audible.is_empty()) && (active || width >= TAB_CLOSE_MIN_WIDTH);
            let label_width = width - if show_close { TAB_CLOSE_WIDTH } else { 0 } - if show_sound { TAB_CLOSE_WIDTH } else { 0 };
            let chars = ((label_width - 20) / TAB_CHAR_WIDTH).max(2) as usize;
            t.button.set_text(Some(&CefString::from(t.label(chars).as_str())));
            t.button.set_minimum_size(Some(&Size { width: label_width, height: 0 }));
            t.button.set_maximum_size(Some(&Size { width: label_width, height: 100 }));
            Button::from(&t.button).set_tooltip_text(Some(&CefString::from(self.tab_tooltip(t).as_str())));
            View::from(&t.button).set_background_color(if active { ACTIVE_TAB } else { 0 });
            View::from(&t.button).set_visible(visible as _);
            View::from(&t.close).set_visible(show_close as _);
            View::from(&t.sound).set_visible(show_sound as _);
            t.sound.set_text(Some(&CefString::from(if t.muted { "🔇" } else { "🔊" })));
            Button::from(&t.sound).set_tooltip_text(Some(&CefString::from(if t.muted { "Unmute tab" } else { "Mute tab" })));
        }
        let hidden = self.tabs.len() - fit;
        View::from(&self.overflow).set_visible((hidden > 0) as _);
        self.overflow.set_text(Some(&CefString::from(format!("▾ {hidden}").as_str())));
        self.strip.layout();
    }

    /// Title plus memory use, like Chrome's tab hover card.
    fn tab_tooltip(&self, t: &Tab) -> String {
        let mut tip = t.title_text().to_string();
        if let (Some(bytes), Some(pid)) = (t.memory, t.pid) {
            tip.push_str(&format!("
Memory usage: {}", memory::format_bytes(bytes)));
            let others = self.tabs.iter().filter(|o| o.id != t.id && o.pid == Some(pid)).count();
            if others > 0 {
                tip.push_str(&format!(" (shared with {others} other tab{})", if others == 1 { "" } else { "s" }));
            }
        }
        tip
    }

    pub fn on_renderer_pid(&mut self, browser_id: i32, pid: u32) {
        if let Some(t) = self.tab_for_browser(browser_id) {
            t.pid = Some(pid);
            t.memory = memory::private_bytes(pid);
        }
        self.layout_tabs();
    }

    /// Re-reads every tab's memory use (every few seconds).
    fn sample_memory(&mut self) {
        for t in &mut self.tabs {
            t.memory = t.pid.and_then(memory::private_bytes);
        }
        self.layout_tabs();
    }

    /// The "▾ N" button: a menu listing every tab.
    fn show_tab_menu(&self) {
        let Some(menu) = menu_model_create(Some(&mut TabMenu::new())) else { return };
        for (i, t) in self.tabs.iter().enumerate() {
            let id = TAB_MENU_BASE + i as c_int;
            let sound = if t.muted { "🔇 " } else if t.audible.is_empty() { "" } else { "🔊 " };
            menu.add_check_item(id, Some(&CefString::from(format!("{sound}{}", t.label(60)).as_str())));
            menu.set_checked(id, (i == self.active) as _);
        }
        let b = View::from(&self.overflow).bounds_in_screen();
        let mut menu = menu;
        self.window.show_menu(Some(&mut menu), Some(&Point { x: b.x, y: b.y + b.height }), MenuAnchorPosition::TOPLEFT);
    }

    /// Moves the active tab one place left or right (Ctrl+Shift+PageUp / PageDown).
    fn move_tab(&mut self, right: bool) {
        let i = self.active;
        let j = if right { i + 1 } else { i.wrapping_sub(1) };
        if j >= self.tabs.len() {
            return;
        }
        self.tabs.swap(i, j);
        self.active = j;
        // Tab k's label and ✕ sit at strip positions 2k and 2k+1.
        for (k, t) in self.tabs.iter().enumerate() {
            self.strip.reorder_child_view(Some(&mut View::from(&t.button)), 3 * k as c_int);
            self.strip.reorder_child_view(Some(&mut View::from(&t.sound)), 3 * k as c_int + 1);
            self.strip.reorder_child_view(Some(&mut View::from(&t.close)), 3 * k as c_int + 2);
        }
        self.refresh();
    }

    /// Redraws tab labels, the address bar, buttons and the shield from current state.
    pub fn refresh(&mut self) {
        self.layout_tabs();
        self.fit_bookmarks_bar();
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
        View::from(&self.update).set_visible((matches!(update::status(), update::Status::Ready { .. }) || crate::engine::is_ready()) as _);

        // 📖 on pages that look like articles, and in reader view (to leave it).
        let (readerable, in_reader) = self.tabs.get(self.active).map_or((false, false), |t| (t.readerable && t.page.is_none(), t.in_reader));
        View::from(&self.reader).set_visible((readerable || in_reader) as _);
        self.reader.set_text(Some(&CefString::from(if in_reader { "📖 Leave reader" } else { "📖" })));
        Button::from(&self.reader).set_tooltip_text(Some(&CefString::from(if in_reader { "Back to the original page (F9)" } else { "Reader view (F9)" })));

        // 🔑 on sites with saved logins.
        let has_logins = self.active_origin().is_some_and(|o| passwords::has_logins(&o));
        View::from(&self.key).set_visible(has_logins as _);

        // ☆ / ★ for web pages only.
        let (can_bookmark, bookmarked) = match self.tabs.get(self.active) {
            Some(t) if t.page.is_none() && history::recordable(&t.url) => (true, bookmarks::is_bookmarked(&t.url)),
            _ => (false, false),
        };
        View::from(&self.star).set_visible(can_bookmark as _);
        self.star.set_text(Some(&CefString::from(if bookmarked { "★" } else { "☆" })));
        Button::from(&self.star).set_tooltip_text(Some(&CefString::from(if bookmarked { "Remove bookmark (Ctrl+D)" } else { "Bookmark this page (Ctrl+D)" })));

        let percent = self.active_host().map_or(100, |h| zoom_percent(h.zoom_level()));
        View::from(&self.zoom).set_visible((percent != 100) as _);
        self.zoom.set_text(Some(&CefString::from(format!("{percent}%").as_str())));

        let mode = if self.private { " (Private)" } else { "" };
        let title = format!("{} - Bare{mode}", active.title_text().chars().take(80).collect::<String>());
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
        let private = self.private;
        if let Some(t) = self.tab_for_browser(browser_id) {
            if !private && t.page.is_none() {
                history::set_title(&t.url, &title);
            }
            t.title = title;
        }
        self.refresh();
    }

    pub fn on_url(&mut self, browser_id: i32, url: String) {
        let private = self.private;
        if let Some(t) = self.tab_for_browser(browser_id) {
            // Requests from a site the tab has left are no longer relevant.
            let host = |u: &str| url::host(&u.to_ascii_lowercase()).map(String::from);
            if host(&t.url) != host(&url) {
                for request in t.prompts.drain(..) {
                    request.answer(false);
                }
            }
            // A different page (not just a new #fragment) needs checking for an article again.
            let page_of = |u: &str| u.split('#').next().unwrap_or("").to_string();
            if page_of(&t.url) != page_of(&url) {
                t.readerable = false;
            }
            // The reader page shows the address of the page it came from, and isn't a visit.
            t.in_reader = t.reader.as_ref().is_some_and(|(page, _)| *page == url);
            if t.in_reader {
                t.page = None;
                t.url = t.reader.as_ref().map(|(_, source)| source.clone()).unwrap_or_default();
                return self.refresh();
            }
            match t.page {
                // Built-in pages show their bare:// address, not the data: URL.
                Some(page) if pages::is_page_url(&url) => t.url = page.display_url(),
                _ => {
                    t.page = None;
                    let new = if url == url::BLANK || pages::is_page_url(&url) { String::new() } else { url };
                    // A visit (not private, and not just the same address reported again).
                    if !private && new != t.recorded && history::recordable(&new) {
                        history::record_visit(&new, &t.title);
                        t.recorded = new.clone();
                    }
                    t.url = new;
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
        self.refresh_save_bar();
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

    impl ViewDelegate {
        /// Close the suggestions when focus leaves the address bar (after a moment, so a
        /// click on a suggestion still lands).
        fn on_blur(&self, _view: Option<&mut View>) {
            defer_after(200, |ui| ui.hide_suggestions());
        }
    }

    impl TextfieldDelegate {
        fn on_key_event(&self, textfield: Option<&mut Textfield>, event: Option<&KeyEvent>) -> c_int {
            let (Some(field), Some(e)) = (textfield, event) else { return 0 };
            if e.type_ != KeyEventType::RAWKEYDOWN {
                return 0;
            }
            match e.windows_key_code {
                0x0D => {
                    // Enter: the highlighted suggestion, or what's typed
                    let input = CefString::from(&field.text()).to_string();
                    with_ui(move |ui| ui.enter_address(&input));
                    1
                }
                0x26 | 0x28 => {
                    // Up / Down: move through the suggestions
                    let down = e.windows_key_code == 0x28;
                    with_ui(move |ui| ui.move_suggestion(down));
                    1
                }
                0x1B => {
                    // Escape: close the suggestions, or restore the page's address
                    with_ui(|ui| {
                        if ui.suggestions_visible() {
                            ui.hide_suggestions();
                            return;
                        }
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

        /// Suggest as you type.
        fn on_after_user_action(&self, _textfield: Option<&mut Textfield>) {
            with_ui(|ui| ui.update_suggestions());
        }
    }
}

/// Menu command ids for a bookmarks-bar folder are BOOKMARK_MENU_BASE + index into `menu_urls`.
const BOOKMARK_MENU_BASE: c_int = 5000;
/// Menu command ids for the 🔑 menu are LOGIN_MENU_BASE + index into `menu_logins`.
const LOGIN_MENU_BASE: c_int = 7000;
/// Sub-menu ids (folders inside the "»" menu).
const BOOKMARK_SUBMENU_BASE: c_int = 9000;

wrap_menu_model_delegate! {
    struct BookmarkMenu {}

    impl MenuModelDelegate {
        fn execute_command(&self, _menu_model: Option<&mut MenuModel>, command_id: c_int, _event_flags: EventFlags) {
            if let Ok(index) = usize::try_from(command_id - BOOKMARK_MENU_BASE) {
                defer(move |ui| ui.open_menu_url(index));
            }
        }
    }
}

/// Menu command ids for the "▾ N" tab list are TAB_MENU_BASE + tab index.
const TAB_MENU_BASE: c_int = 3000;

wrap_run_file_dialog_callback! {
    struct CsvChosen;

    impl RunFileDialogCallback {
        fn on_file_dialog_dismissed(&self, file_paths: Option<&mut CefStringList>) {
            // Take the list itself: cloning a list CEF lent us yields an empty one.
            let Some(path) = file_paths.and_then(|list| std::mem::take(list).into_iter().next()) else { return };
            defer(move |ui| ui.import_password_csv(path.into()));
        }
    }
}

wrap_menu_model_delegate! {
    struct LoginMenu {}

    impl MenuModelDelegate {
        fn execute_command(&self, _menu_model: Option<&mut MenuModel>, command_id: c_int, _event_flags: EventFlags) {
            if let Ok(index) = usize::try_from(command_id - LOGIN_MENU_BASE) {
                defer(move |ui| ui.fill_login(index));
            }
        }
    }
}

wrap_menu_model_delegate! {
    struct TabMenu {}

    impl MenuModelDelegate {
        fn execute_command(&self, _menu_model: Option<&mut MenuModel>, command_id: c_int, _event_flags: EventFlags) {
            if let Ok(index) = usize::try_from(command_id - TAB_MENU_BASE) {
                defer(move |ui| ui.select(index));
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

        /// Lets Bare's own "Picture in picture" command (Alt+P, right-click menu) start PiP;
        /// pages still need a click to do it themselves.
        fn allow_picture_in_picture_without_user_activation(&self, _browser_view: Option<&mut BrowserView>) -> c_int {
            1
        }

        /// The bindings default to a PiP window that can't be dragged around.
        fn allow_move_for_picture_in_picture(&self, _browser_view: Option<&mut BrowserView>) -> c_int {
            1
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
            with_ui(|ui| ui.rebuild_bookmarks_bar());
            with_ui(move |ui| ui.open_target(&start));
            window.show();
            // Check for updates shortly after startup (at most once a day, if enabled).
            post_delayed_task(ThreadId::UI, Some(&mut UpdateTimer::new()), 15_000);
            post_delayed_task(ThreadId::UI, Some(&mut MemoryTimer::new()), MEMORY_SAMPLE_MS);
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

        /// Re-fit the tab strip when the window is resized.
        fn on_window_bounds_changed(&self, _window: Option<&mut Window>, _new_bounds: Option<&Rect>) {
            defer(|ui| ui.refresh());
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
    fn tab_strip_layout() {
        // Few tabs: full width, all visible.
        assert_eq!(tab_layout(3, 0, 0, 1000), (TAB_MAX_WIDTH, 0, 3));
        // Many tabs shrink, but not below the minimum.
        let (w, first, fit) = tab_layout(10, 9, 0, 1000);
        assert_eq!((w, fit), (100, 10));
        assert_eq!(first, 0);
        // Too many: the visible window slides so the active tab is shown.
        let (w, first, fit) = tab_layout(40, 35, 0, 1000);
        assert_eq!(w, TAB_MIN_WIDTH);
        assert_eq!(fit, (1000 / TAB_MIN_WIDTH) as usize);
        assert!(first <= 35 && 35 < first + fit);
        // Going back to an earlier tab scrolls back.
        let (_, first2, _) = tab_layout(40, 2, first, 1000);
        assert!(first2 <= 2);
        assert_eq!(tab_layout(0, 0, 0, 1000).2, 0);
    }

    #[test]
    fn zoom_levels() {
        assert_eq!(zoom_percent(0.0), 100);
        assert_eq!(zoom_percent(1.0), 120);
        assert_eq!(zoom_percent(-1.0), 83);
        // Chromium's preset 125% is stored as log(1.25)/log(1.2).
        assert_eq!(zoom_percent(1.25f64.ln() / 1.2f64.ln()), 125);
    }

    #[test]
    fn find_counter() {
        assert_eq!(find_label(0, 0), "No results");
        assert_eq!(find_label(1, 0), "1 match");
        assert_eq!(find_label(2, 0), "2 matches");
        assert_eq!(find_label(2, 1), "1 of 2");
        assert_eq!(find_label(12, 12), "12 of 12");
    }
}
