//! Freedesktop desktop-entry metadata, writing, and desktop-cache refreshes.

use std::{fs, path::{Path, PathBuf}};
use anyhow::{Context, Result};
use crate::{host, security::safe_desktop_value, utils};

/// Safe, optional metadata read from a package's desktop entry.
#[derive(Clone, Debug, Default)]
pub struct DesktopMetadata {
    pub source: PathBuf,
    pub name: Option<String>, pub icon: Option<String>, pub categories: Option<String>,
    pub comment: Option<String>, pub terminal: Option<bool>, pub startup_wm_class: Option<String>,
    pub startup_notify: Option<bool>, pub mime_type: Option<String>,
}

/// Values used for TarDrop's generated launcher. The executable itself is always TarDrop-validated.
pub struct DesktopEntry<'a> {
    pub name: &'a str, pub comment: Option<&'a str>, pub executable: &'a Path, pub icon: Option<&'a Path>,
    pub categories: &'a str, pub terminal: bool, pub startup_notify: bool, pub startup_wm_class: Option<&'a str>, pub mime_type: Option<&'a str>, pub id: &'a str,
}

/// Parses fields that can enrich a generated entry. Invalid values are discarded, not propagated.
pub fn read_metadata(path: &Path) -> Option<DesktopMetadata> {
    let text = fs::read_to_string(path).ok()?;
    let mut metadata = DesktopMetadata { source: path.to_path_buf(), ..Default::default() };
    let mut in_entry = false;
    for line in text.lines() {
        if line == "[Desktop Entry]" { in_entry = true; continue; }
        if line.starts_with('[') { in_entry = false; }
        if !in_entry || line.starts_with('#') { continue; }
        let Some((key, value)) = line.split_once('=') else { continue; };
        if safe_desktop_value(value).is_err() { continue; }
        match key {
            "Name" => metadata.name = Some(value.to_owned()),
            "Icon" => metadata.icon = Some(value.to_owned()),
            "Categories" if valid_list(value) => metadata.categories = Some(value.to_owned()),
            "Comment" => metadata.comment = Some(value.to_owned()),
            "Terminal" => metadata.terminal = match value { "true" | "True" => Some(true), "false" | "False" => Some(false), _ => None },
            "StartupNotify" => metadata.startup_notify = match value { "true" | "True" => Some(true), "false" | "False" => Some(false), _ => None },
            "StartupWMClass" => metadata.startup_wm_class = Some(value.to_owned()),
            "MimeType" if valid_list(value) => metadata.mime_type = Some(value.to_owned()),
            _ => {},
        }
    }
    Some(metadata)
}

/// Writes a conservative, specification-shaped launcher after validating every copied value.
pub fn write(entry: DesktopEntry<'_>) -> Result<PathBuf> {
    for value in [entry.name, entry.categories, entry.id] { safe_desktop_value(value)?; }
    for value in [entry.comment, entry.startup_wm_class, entry.mime_type].into_iter().flatten() { safe_desktop_value(value)?; }
    let target = utils::applications_database_dir()?.join(format!("{}.desktop", entry.id));
    let mut content = format!("[Desktop Entry]\nVersion=1.5\nType=Application\nName={}\n", entry.name);
    if let Some(comment) = entry.comment { content.push_str(&format!("Comment={comment}\n")); }
    content.push_str(&format!("Exec={}\n", quote_argument(entry.executable)));
    if let Some(icon) = entry.icon { content.push_str(&format!("Icon={}\n", icon.to_string_lossy().replace('\n', ""))); }
    // StartupNotify is true only when the packaged entry explicitly declares support. Directly
    // launching a portable process normally cannot send the completion signal, which makes
    // Plasma show a misleading bouncing cursor until it times out.
    content.push_str(&format!("Terminal={}\nCategories={}\nStartupNotify={}\n", entry.terminal, entry.categories, entry.startup_notify));
    if let Some(wm_class) = entry.startup_wm_class { content.push_str(&format!("StartupWMClass={wm_class}\n")); }
    if let Some(mime_type) = entry.mime_type { content.push_str(&format!("MimeType={mime_type}\n")); }
    fs::write(&target, content).with_context(|| format!("could not write {}", target.display()))?;
    Ok(target)
}

/// Only accepts semicolon-separated desktop lists, preventing arbitrary newline-style injection.
fn valid_list(value: &str) -> bool { !value.is_empty() && value.ends_with(';') && value.split(';').all(|part| part.is_empty() || part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | '+'))) }

/// Quotes an absolute path because archive names can legitimately contain spaces.
fn quote_argument(path: &Path) -> String { format!("\"{}\"", path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"")) }

/// Refreshes caches when the associated desktop tools are present; all failures are non-fatal.
pub fn refresh_integrations() {
    let applications = utils::applications_database_dir().unwrap_or_default();
    let icon_root = dirs::data_local_dir().unwrap_or_default().join("icons");
    let _ = host::command("update-desktop-database").arg(applications).status();
    let _ = host::command("gtk-update-icon-cache").args(["-f", "-t"]).arg(icon_root).status();
    let _ = host::command("kbuildsycoca6").status();
}
