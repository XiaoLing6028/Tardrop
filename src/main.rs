//! TarDrop is a user-local, security-conscious installer for portable archives.
//!
//! The GUI stays deliberately small; the installer modules do all security-sensitive work.

#![forbid(unsafe_code)]

mod archive;
mod desktop;
mod icons;
mod installer;
mod security;
mod ui;
mod updates;
mod utils;

use adw::prelude::*;
use gtk4::glib;

/// Reverse-DNS application id GTK uses for the Wayland app-id, so the shell matches the window to
/// TarDrop's own desktop entry instead of showing a generic placeholder in the task switcher.
const APP_ID: &str = "org.tardrop.TarDrop";

/// Starts the desktop application and reports failures to the terminal too.
///
/// `adw::Application` is used rather than `gtk::Application` because it initialises libadwaita's
/// style manager, which is what makes the window follow the desktop's light/dark preference and
/// accent colour instead of shipping its own hardcoded palette.
fn main() -> glib::ExitCode {
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_activate(ui::build_ui);
    // GTK parses its own switches; passing none keeps archive paths from being read as options.
    application.run_with_args::<&str>(&[])
}
