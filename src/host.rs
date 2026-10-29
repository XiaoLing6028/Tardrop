//! Programs TarDrop starts run in the desktop's environment, not in the one TarDrop was given.
//!
//! Packaged as an AppImage, TarDrop is started by `AppRun`, which points GTK at the icons, schemas,
//! image loaders, and GIO modules inside the image (`XDG_DATA_DIRS`, `GDK_PIXBUF_MODULE_FILE`,
//! `GIO_MODULE_DIR`), and the AppImage runtime adds `APPIMAGE`, `APPDIR`, `ARGV0`, and `OWD`.
//! Inherited, those would make `xdg-open`, the desktop-cache tools, and every installed application
//! load TarDrop's private copies instead of the host's, index TarDrop's internal launcher from a
//! mount that disappears on exit, and let an installed Electron app's updater mistake TarDrop's
//! `$APPIMAGE` for its own and overwrite it. `AppRun` therefore lists what it changed in
//! `TARDROP_HOST_ENV` and saves each prior value as `TARDROP_HOST_<NAME>`, and every child gets
//! those back. A native build never sets `TARDROP_HOST_ENV`, so there this is a plain `Command`.

use std::{ffi::{OsStr, OsString}, process::Command};

/// Space-separated names of the variables the AppImage changed; absent in a native build.
const CHANGED: &str = "TARDROP_HOST_ENV";

/// Prefix of the variable holding a changed variable's value from before `AppRun`; absent if unset.
const SAVED: &str = "TARDROP_HOST_";

/// A command for a host program or an installed application, in the host's own environment.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    restore(&mut command, |name| std::env::var_os(name));
    command
}

/// Puts back every variable named in `TARDROP_HOST_ENV`, or removes it when it was originally unset,
/// and hides the bookkeeping itself. `lookup` reads TarDrop's environment, so tests can supply one.
fn restore(command: &mut Command, lookup: impl Fn(&str) -> Option<OsString>) {
    let Some(changed) = lookup(CHANGED) else { return };
    command.env_remove(CHANGED);
    // Only plain variable names are honoured, so a malformed list cannot address anything else.
    let names = changed.to_string_lossy().split_whitespace().filter(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).map(str::to_owned).collect::<Vec<_>>();
    for name in names {
        let saved = format!("{SAVED}{name}");
        match lookup(&saved) { Some(value) => { command.env(&name, value); } None => { command.env_remove(&name); } }
        command.env_remove(saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The environment changes `restore` makes, as name → value (`None` means removed).
    fn changes(environment: &[(&str, &str)]) -> BTreeMap<String, Option<String>> {
        let environment: BTreeMap<_, _> = environment.iter().map(|(name, value)| (name.to_string(), OsString::from(value))).collect();
        let mut command = Command::new("true");
        restore(&mut command, |name| environment.get(name).cloned());
        command.get_envs().map(|(name, value)| (name.to_string_lossy().into_owned(), value.map(|value| value.to_string_lossy().into_owned()))).collect()
    }

    #[test]
    fn a_native_build_passes_the_environment_through_untouched() {
        assert!(changes(&[("XDG_DATA_DIRS", "/usr/share"), ("TARDROP_HOST_XDG_DATA_DIRS", "/elsewhere")]).is_empty());
    }

    #[test]
    fn appimage_changes_are_undone_for_children() {
        let changes = changes(&[
            ("TARDROP_HOST_ENV", "XDG_DATA_DIRS GIO_MODULE_DIR APPIMAGE"),
            ("XDG_DATA_DIRS", "/tmp/.mount_x/usr/share:/usr/share"), ("TARDROP_HOST_XDG_DATA_DIRS", "/usr/share"),
            ("GIO_MODULE_DIR", "/tmp/.mount_x/usr/lib/gio/modules"), ("APPIMAGE", "/home/me/TarDrop.AppImage"),
        ]);
        let expected: BTreeMap<String, Option<String>> = [
            ("TARDROP_HOST_ENV", None), ("XDG_DATA_DIRS", Some("/usr/share")), ("TARDROP_HOST_XDG_DATA_DIRS", None),
            ("GIO_MODULE_DIR", None), ("TARDROP_HOST_GIO_MODULE_DIR", None), ("APPIMAGE", None), ("TARDROP_HOST_APPIMAGE", None),
        ].into_iter().map(|(name, value)| (name.to_owned(), value.map(str::to_owned))).collect();
        assert_eq!(changes, expected);
    }

    #[test]
    fn a_value_that_was_set_but_empty_stays_set_and_empty() {
        assert_eq!(changes(&[("TARDROP_HOST_ENV", "GIO_MODULE_DIR"), ("TARDROP_HOST_GIO_MODULE_DIR", "")])["GIO_MODULE_DIR"], Some(String::new()));
    }

    #[test]
    fn only_plain_variable_names_are_restored() {
        let changes = changes(&[("TARDROP_HOST_ENV", "LD_PRELOAD=x PATH;rm OWD"), ("TARDROP_HOST_PATH;rm", "/evil")]);
        assert_eq!(changes.keys().map(String::as_str).collect::<Vec<_>>(), ["OWD", "TARDROP_HOST_ENV", "TARDROP_HOST_OWD"]);
    }
}
