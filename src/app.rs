//! Process startup, CEF settings and privacy defaults.

use std::path::PathBuf;

use cef::*;

use crate::{block, handlers, network, permissions, settings, ui, update, url};

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
    update::init(&root);

    let settings = Settings {
        no_sandbox: cfg!(all(windows, not(feature = "sandbox"))) as _,
        root_cache_path: CefString::from(root.to_string_lossy().as_ref()),
        cache_path: CefString::from(root.join("Default").to_string_lossy().as_ref()),
        // No debug.log next to the executable.
        log_severity: LogSeverity::DISABLE,
        // Dev tools and other CEF-owned popups get a default window.
        use_views_default_popup: 1,
        ..Default::default()
    };
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), sandbox_info) != 1 {
        return 1;
    }
    run_message_loop();
    shutdown();
    update::apply_if_staged();
    0
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
    }
}

wrap_browser_process_handler! {
    struct BareProcessHandler;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            let cl = command_line_get_global();
            let has = |s: &str| cl.as_ref().is_some_and(|c| c.has_switch(Some(&CefString::from(s))) == 1);
            let private = has("private");

            // First non-switch argument is a URL or search to open.
            let mut start = url::BLANK.to_string();
            if let Some(cl) = cl.as_ref() {
                let mut list = CefStringList::new();
                cl.arguments(Some(&mut list));
                if let Some(first) = list.into_iter().next() {
                    // bare://settings etc. are opened as built-in pages by the window.
                    let page = first.to_ascii_lowercase().starts_with("bare://");
                    start = if page { first } else { url::resolve(&first, settings::get().search.prefix()) };
                }
            }

            // Private mode: an in-memory profile (empty cache path), discarded on exit.
            let mut context = if private {
                request_context_create_context(Some(&RequestContextSettings::default()), None)
            } else {
                request_context_get_global_context()
            };
            if let Some(ctx) = context.as_mut() {
                apply_privacy_defaults(ctx);
                // Undo "Allow until Bare closes" grants left over from a crash.
                ui::reset_session_grants(ctx);
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
