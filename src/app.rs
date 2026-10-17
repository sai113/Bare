//! Process startup, CEF settings and privacy defaults.

use std::path::PathBuf;

use cef::*;

use crate::{block, bookmarks, engine, handlers, history, network, passwords, permissions, settings, system, ui, update};

/// This process is a private window (nothing about browsing is saved).
pub static PRIVATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn is_private() -> bool {
    PRIVATE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Runs one CEF process. The browser process returns when the last window closes; renderer,
/// GPU and utility processes return from `execute_process` immediately.
pub fn run(args: &args::Args, sandbox_info: *mut u8) -> i32 {
    // Pin the CEF C API version this code was written against.
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let Some(cmd_line) = args.as_cmd_line() else { return 1 };
    let mut app = BareApp::new();
    let is_browser = cmd_line.has_switch(Some(&CefString::from("type"))) != 1;
    let code = execute_process(Some(args.as_main_args()), Some(&mut app), sandbox_info);
    if !is_browser {
        return code;
    }

    let root = data_dir();
    let _ = std::fs::create_dir_all(&root);
    settings::init(root.join("settings.txt"));
    if settings::get().wipe_on_start {
        // "Erase all browsing data on next start": the whole Chromium profile, before CEF opens it.
        let _ = std::fs::remove_dir_all(root.join("Default"));
        settings::update(|s| s.wipe_on_start = false);
    }
    block::init(&root, settings::get().extras());
    permissions::init(&root);
    history::init(&root);
    bookmarks::init(&root);
    passwords::init(&root);
    update::init(&root);
    engine::init();

    // A private window runs as its own Bare process with a throwaway Chromium folder, so it
    // never hands off to (or shares anything with) an already open normal window.
    let private = cmd_line.has_switch(Some(&CefString::from("private"))) == 1;
    PRIVATE.store(private, std::sync::atomic::Ordering::Relaxed);
    let (cef_root, cache) = if private {
        let tmp = std::env::temp_dir().join(format!("bare-private-{}", std::process::id()));
        (tmp, String::new())
    } else {
        (root.clone(), root.join("Default").to_string_lossy().into_owned())
    };
    let settings = Settings {
        no_sandbox: cfg!(all(windows, not(feature = "sandbox"))) as _,
        root_cache_path: CefString::from(cef_root.to_string_lossy().as_ref()),
        cache_path: CefString::from(cache.as_str()),
        // No debug.log next to the executable.
        log_severity: LogSeverity::DISABLE,
        // Dev tools and other CEF-owned popups get a default window.
        use_views_default_popup: 1,
        ..Default::default()
    };
    // Fails (returns 0) when another Bare is already running: CEF has then passed our command
    // line to it (see `on_already_running_app_relaunch`), which opens it in its window.
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), sandbox_info) != 1 {
        return 0;
    }
    run_message_loop();
    shutdown();
    if private {
        let _ = std::fs::remove_dir_all(&cef_root);
    }
    update::apply_if_staged();
    0
}

/// URLs, files or searches given on a command line (everything that isn't a --switch).
fn launch_targets(cl: &CommandLine) -> Vec<String> {
    let mut list = CefStringList::new();
    cl.arguments(Some(&mut list));
    list.into_iter().filter(|a| !a.trim().is_empty()).map(|a| system::launch_target(&a)).collect()
}

/// Per-user profile folder.
fn data_dir() -> PathBuf {
    let env = |k| std::env::var_os(k).map(PathBuf::from);
    if cfg!(windows) {
        env("LOCALAPPDATA").unwrap_or_default().join("Bare")
    } else if cfg!(target_os = "macos") {
        env("HOME").unwrap_or_default().join("Library/Application Support/Bare")
    } else {
        env("XDG_DATA_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".local/share")))
            .unwrap_or_default()
            .join("bare")
    }
}

wrap_app! {
    struct BareApp;

    impl App {
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let is_browser = process_type.is_none_or(|t| t.to_string().is_empty());
            let Some(cl) = command_line else { return };
            if !is_browser {
                return;
            }
            // Turn off Chromium background services that phone home or that a minimal browser doesn't need.
            // disable-extensions also stops other software from side-loading Chrome extensions via the registry.
            for switch in ["disable-background-networking", "disable-component-update", "disable-domain-reliability", "no-default-browser-check", "no-first-run", "disable-extensions"] {
                cl.append_switch(Some(&CefString::from(switch)));
            }
            // Append to (never replace) the features CEF itself disables, or Chromium crashes.
            let name = CefString::from("disable-features");
            let existing = CefString::from(&cl.switch_value(Some(&name))).to_string();
            let ours = "MediaRouter,Translate,OptimizationHints,AutofillServerCommunication";
            let merged = if existing.is_empty() { ours.to_string() } else { format!("{existing},{ours}") };
            cl.append_switch_with_value(Some(&name), Some(&CefString::from(merged.as_str())));
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(BareProcessHandler::new())
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(RenderHandler::new())
        }
    }
}

/// Name of the message a page's process sends with its process id (for the tab's memory use).
pub const PID_MESSAGE: &str = "bare-pid";

wrap_render_process_handler! {
    struct RenderHandler;

    impl RenderProcessHandler {
        /// Runs in the (sandboxed) page process each time a page starts: tell the browser which
        /// process is showing this tab.
        fn on_context_created(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, _context: Option<&mut V8Context>) {
            let Some(frame) = frame else { return };
            if frame.is_main() != 1 {
                return;
            }
            let Some(mut msg) = process_message_create(Some(&CefString::from(PID_MESSAGE))) else { return };
            if let Some(args) = msg.argument_list() {
                args.set_int(0, std::process::id() as _);
            }
            frame.send_process_message(ProcessId::BROWSER, Some(&mut msg));
        }
    }
}

wrap_browser_process_handler! {
    struct BareProcessHandler;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            let cl = command_line_get_global();
            let has = |s: &str| cl.as_ref().is_some_and(|c| c.has_switch(Some(&CefString::from(s))) == 1);
            let private = has("private");

            // The first link/file/search given on the command line; others open as more tabs.
            let mut targets = cl.as_ref().map(launch_targets).unwrap_or_default();
            let start = if targets.is_empty() { ui::NEW_TAB.to_string() } else { targets.remove(0) };
            for extra in targets {
                ui::defer(move |ui| ui.open_target(&extra));
            }

            // Private mode: an in-memory profile (empty cache path), discarded on exit.
            let mut context = if private {
                request_context_create_context(Some(&RequestContextSettings::default()), None)
            } else {
                request_context_get_global_context()
            };
            if let Some(ctx) = context.as_mut() {
                apply_privacy_defaults(ctx);
                if !private {
                    // Undo "Allow until Bare closes" grants left over from a crash.
                    ui::reset_session_grants(ctx);
                }
            }
            {
                let s = settings::get();
                let _ = network::apply_dns(&s.dns);
                if let Some(ctx) = context.as_ref() {
                    let _ = network::apply_proxy(ctx, &s.proxy);
                }
            }

            let mut delegate = ui::MainWindow::new(handlers::client(), context, start, private);
            window_create_top_level(Some(&mut delegate));
        }

        /// Bare was started again while running (e.g. a link clicked in another app, now that
        /// Bare can be the default browser): open what it was given here, in new tabs.
        fn on_already_running_app_relaunch(&self, command_line: Option<&mut CommandLine>, _current_directory: Option<&CefString>) -> ::std::os::raw::c_int {
            let targets = command_line.as_ref().map(|cl| launch_targets(cl)).unwrap_or_default();
            ui::defer(move |ui| {
                if targets.is_empty() {
                    ui.open_target(ui::NEW_TAB);
                }
                for t in &targets {
                    ui.open_target(t);
                }
                ui.bring_to_front();
            });
            1
        }
    }
}

fn apply_privacy_defaults(ctx: &mut RequestContext) {
    let set = |name: &str, value: Option<Value>| {
        if let Some(mut v) = value {
            // Always pass an error string: in release builds the call fails without one.
            let mut error = CefString::from("");
            ctx.set_preference(Some(&CefString::from(name)), Some(&mut v), Some(&mut error));
        }
    };
    let boolean = |b: bool| value_create().inspect(|v| { v.set_bool(b as _); });
    let int = |i: i32| value_create().inspect(|v| { v.set_int(i); });

    set("profile.cookie_controls_mode", int(1)); // block third-party cookies
    set("credentials_enable_service", boolean(false)); // no password saving
    set("profile.password_manager_enabled", boolean(false));
    set("autofill.profile_enabled", boolean(false));
    set("autofill.credit_card_enabled", boolean(false));
    set("translate.enabled", boolean(false));
    set("search.suggest_enabled", boolean(false));
    // Sites can't show notifications.
    ctx.set_content_setting(None, None, ContentSettingTypes::NOTIFICATIONS, ContentSettingValues::BLOCK);
}
