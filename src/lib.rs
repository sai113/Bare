//! Windows sandboxed build: CEF's `bootstrap.exe` (shipped as `Bare.exe`) loads this DLL and
//! calls `RunWinMain` in every process (browser, renderer, GPU, ...), each inside the sandbox.

#[cfg(all(windows, feature = "sandbox"))]
mod app;
#[cfg(all(windows, feature = "sandbox"))]
mod audio;
#[cfg(all(windows, feature = "sandbox"))]
mod block;
#[cfg(all(windows, feature = "sandbox"))]
mod bookmarks;
#[cfg(all(windows, feature = "sandbox"))]
mod downloads;
#[cfg(all(windows, feature = "sandbox"))]
mod engine;
#[cfg(all(windows, feature = "sandbox"))]
mod engine_files;
#[cfg(all(windows, feature = "sandbox"))]
mod handlers;
#[cfg(all(windows, feature = "sandbox"))]
mod hello;
#[cfg(all(windows, feature = "sandbox"))]
mod history;
#[cfg(all(windows, feature = "sandbox"))]
mod memory;
#[cfg(all(windows, feature = "sandbox"))]
mod network;
#[cfg(all(windows, feature = "sandbox"))]
mod pages;
#[cfg(all(windows, feature = "sandbox"))]
mod passwords;
#[cfg(all(windows, feature = "sandbox"))]
mod permissions;
#[cfg(all(windows, feature = "sandbox"))]
mod reader;
#[cfg(all(windows, feature = "sandbox"))]
mod settings;
#[cfg(all(windows, feature = "sandbox"))]
mod system;
#[cfg(all(windows, feature = "sandbox"))]
mod ui;
#[cfg(all(windows, feature = "sandbox"))]
mod update;
#[cfg(all(windows, feature = "sandbox"))]
mod url;
#[cfg(all(windows, feature = "sandbox"))]
mod winres;

#[cfg(all(windows, feature = "sandbox"))]
#[unsafe(no_mangle)]
unsafe extern "C" fn RunWinMain(
    instance: cef::sys::HINSTANCE,
    _command_line: *const u8,
    _command_show: i32,
    sandbox_info: *mut u8,
) -> i32 {
    let args = cef::args::Args::from(cef::MainArgs { instance });
    app::run(&args, sandbox_info)
}
