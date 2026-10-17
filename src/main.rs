//! Executable entry point for Linux/macOS, and for Windows builds without the sandbox
//! (`cargo run --no-default-features`, handy for development).

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod block;
mod bookmarks;
mod downloads;
mod handlers;
mod history;
mod memory;
mod network;
mod pages;
mod passwords;
mod permissions;
mod reader;
mod settings;
mod system;
mod ui;
mod update;
mod url;

#[cfg(not(all(windows, feature = "sandbox")))]
fn main() {
    let args = cef::args::Args::new();
    std::process::exit(app::run(&args, std::ptr::null_mut()));
}

#[cfg(all(windows, feature = "sandbox"))]
fn main() {
    eprintln!("The sandboxed Windows build runs through Bare.exe: use `cargo xtask package`.");
    std::process::exit(1);
}
