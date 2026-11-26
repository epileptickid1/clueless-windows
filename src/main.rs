//! Clueless: a translucent meeting assistant that uses signed-in model CLIs.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod antigravity;
mod audio;
mod backend;
mod claude;
mod codex;
mod config;
mod event;
mod files;
mod gate;
mod kwin;
mod question;
mod render;
mod screen;
mod stt;
mod ui;
#[cfg(windows)]
mod windows;

use gtk::gio;
use gtk::prelude::*;
use gtk4 as gtk;

const APP_ID: &str = "io.github.clueless";

fn main() -> gtk::glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::default())
        .build();
    app.connect_activate(|app| match app.windows().first() {
        Some(window) => window.present(),
        None => {
            ui::build(app);
        }
    });
    // Settings are saved and child processes stopped when the window closes.
    app.run()
}
