//! The installation transaction: extract privately, inspect, then publish atomically.

use std::{collections::BTreeMap, fs, path::{Component, Path, PathBuf}};
use anyhow::{bail, Context, Result};
use tempfile::Builder;
use walkdir::WalkDir;
use crate::{archive, desktop, icons, security::{self, Policy}, updates, utils};

/// A completed installation, retained by the UI for launch/open/uninstall actions.
#[derive(Clone, Debug)]
pub struct InstalledApp { pub name: String, pub directory: PathBuf, pub executable: PathBuf, pub desktop_file: PathBuf, pub icon: Option<PathBuf>, pub sha256: String }

/// Determines what to do when an owned installation with the same name already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingChoice { Replace, KeepBoth, Cancel }

/// A launcher candidate exposed to the UI when automatic selection would be uncertain.
#[derive(Clone, Debug)]
pub struct LauncherCandidate { pub relative_path: PathBuf, pub score: i32, pub reason: String }

/// An install either completes or asks the user to choose between closely scored launchers.
#[derive(Clone, Debug)]
pub enum InstallResult { Installed(InstalledApp), NeedsLauncherChoice(Vec<LauncherCandidate>) }

/// Installs an archive only below `~/Applications`; all public changes happen after inspection.
/// Content-check failures surface as `security::Rejected`; `Policy::Override` relaxes them on request.
pub fn install(source: &Path, choice: ExistingChoice, selected_launcher: Option<&Path>, policy: Policy, log: &mut Vec<String>) -> Result<InstallResult> {
    let format = archive::detect(source)?;
    let hash = security::archive_sha256(source)?;
    let base_name = utils::archive_stem(source);
    log.push(format!("Validated archive SHA-256: {hash}"));
    let applications = utils::applications_dir()?;
    let staging = Builder::new().prefix(".tardrop-").tempdir_in(&applications).context("could not make private staging directory")?;
    log.push("Extracting archive into private staging directory…".into());
    archive::extract(source, format, staging.path(), policy, log)?;
    let extracted_root = package_root(staging.path());
    log.push("Scoring safe launcher candidates…".into());
    let candidates = executable_candidates(&extracted_root, &base_name, policy)?;
    let executable = match selected_launcher {
        Some(relative) => candidates.iter().find(|candidate| candidate.relative_path == relative)
            .map(|candidate| extracted_root.join(&candidate.relative_path))
            .ok_or_else(|| anyhow::anyhow!("selected launcher is no longer a safe candidate"))?,
        None if candidates.is_empty() && policy == Policy::Enforce => return Err(security::Rejected("No safe application launcher was found.".into()).into()),
        None if candidates.is_empty() => bail!("The archive contains no executable file to launch."),
        None if candidates.len() > 1 && candidates[0].score - candidates[1].score <= 10 => {
            log.push("Top launcher candidates are too close to choose safely; asking you to decide.".into());
            return Ok(InstallResult::NeedsLauncherChoice(candidates));
        }
        None => extracted_root.join(&candidates[0].relative_path),
    };
    log.push(format!("Selected launcher: {}", executable.display()));
    // Metadata is accepted only from a desktop entry that names the selected, validated launcher.
    let metadata = best_desktop_metadata(&extracted_root, &executable)?;
    // A valid package Name is authoritative. Otherwise build a readable label from the actual
    // launcher (rather than leaking filenames such as `tor-browser` into Plasma's UI).
    let raw_executable_name = executable.file_stem().and_then(|name| name.to_str()).unwrap_or_default();
    let generated_name = utils::pretty_name(raw_executable_name);
    let fallback_name = if generated_name.is_empty() { utils::pretty_name(&base_name) } else { generated_name };
    let raw_fallback = if raw_executable_name.is_empty() { base_name } else { raw_executable_name.to_owned() };
    let name = metadata.as_ref().and_then(|metadata| metadata.name.as_deref()).map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned).unwrap_or_else(|| if fallback_name.is_empty() { raw_fallback } else { fallback_name });
    let directory = destination_for(&applications, &name, choice)?;
    if choice == ExistingChoice::Cancel { bail!("Installation cancelled") }
    if directory.exists() && choice == ExistingChoice::Replace {
        // The target was calculated as a direct child of our owned Applications root.
        log.push(format!("Replacing existing TarDrop installation: {}", directory.display()));
        fs::remove_dir_all(&directory).context("could not remove existing managed installation")?;
    }
    let relative_executable = executable.strip_prefix(&extracted_root).context("internal executable path error")?.to_owned();
    let icon_source = icons::find_icon(&extracted_root, &name, metadata.as_ref().and_then(|metadata| metadata.icon.as_deref()), metadata.as_ref().map(|metadata| metadata.source.as_path()));
    log.push("Publishing installation…".into());
    fs::rename(&extracted_root, &directory).context("could not publish installation")?;
    let final_executable = directory.join(relative_executable);
    let id = utils::desktop_id(&name);
    // Copy icon assets into XDG data, rather than referring into a replaceable archive directory.
    let icon = match icon_source {
        Some(source) => {
            let final_source = source.strip_prefix(&extracted_root).map(|relative| directory.join(relative)).unwrap_or(source);
            Some(icons::copy_to_tardrop(&final_source, &id).context("could not copy application icon into XDG data")?)
        }
        None => None,
    };
    let categories = metadata.as_ref().and_then(|metadata| metadata.categories.as_deref()).map(str::to_owned).unwrap_or_else(|| infer_categories(&name));
    log.push("Generating desktop launcher…".into());
    let desktop_file = desktop::write(desktop::DesktopEntry { name: &name, comment: metadata.as_ref().and_then(|metadata| metadata.comment.as_deref()), executable: &final_executable, icon: icon.as_deref(), categories: &categories, terminal: metadata.as_ref().and_then(|metadata| metadata.terminal).unwrap_or(false), startup_notify: metadata.as_ref().and_then(|metadata| metadata.startup_notify).unwrap_or(false), startup_wm_class: metadata.as_ref().and_then(|metadata| metadata.startup_wm_class.as_deref()), mime_type: metadata.as_ref().and_then(|metadata| metadata.mime_type.as_deref()), id: &id })?;
    desktop::refresh_integrations();
    log.push("Done. KDE’s application launcher should now find the application.".into());
    let installed = InstalledApp { name, directory, executable: final_executable, desktop_file, icon, sha256: hash };
    // Persistence remains owned by the update module; the installer reports its completed result.
    updates::record_install(&installed, source).context("could not record installed application")?;
    Ok(InstallResult::Installed(installed))
}

/// Chooses the nearest desktop entry whose safe Exec target equals the selected launcher.
/// This prevents unrelated bundled tools from donating misleading browser/game metadata.
fn best_desktop_metadata(root: &Path, executable: &Path) -> Result<Option<desktop::DesktopMetadata>> {
    let mut best: Option<(usize, desktop::DesktopMetadata)> = None;
    for item in WalkDir::new(root).follow_links(false).into_iter() {
        let item = item?;
        if !item.file_type().is_file() || !item.path().extension().is_some_and(|ext| ext.eq_ignore_ascii_case("desktop")) { continue; }
        if desktop_exec_target(item.path(), root)?.as_deref() != Some(executable) { continue; }
        let Some(metadata) = desktop::read_metadata(item.path()) else { continue; };
        // Prefer package-level metadata over desktop files buried in a component directory.
        let depth = item.path().strip_prefix(root)?.components().count();
        if best.as_ref().is_none_or(|(best_depth, _)| depth < *best_depth) { best = Some((depth, metadata)); }
    }
    Ok(best.map(|(_, metadata)| metadata))
}

/// Supplies useful launcher sections only when the package did not declare its own categories.
fn infer_categories(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if ["browser", "firefox", "tor browser", "chromium", "librewolf"].iter().any(|needle| lower.contains(needle)) { "Network;WebBrowser;".into() }
    else if ["editor", "notepad", "kate", "gedit"].iter().any(|needle| lower.contains(needle)) { "Utility;TextEditor;".into() }
    else if ["ide", "code", "studio", "clion", "idea"].iter().any(|needle| lower.contains(needle)) { "Development;IDE;".into() }
    else if lower.contains("game") { "Game;".into() }
    else if ["player", "vlc", "music", "video"].iter().any(|needle| lower.contains(needle)) { "AudioVideo;Player;".into() }
    else if ["image", "photo", "draw", "graphics"].iter().any(|needle| lower.contains(needle)) { "Graphics;".into() }
    else if ["terminal", "console", "terminator"].iter().any(|needle| lower.contains(needle)) { "System;TerminalEmulator;".into() }
    else { "Utility;".into() }
}

/// Collapses an archive's one enclosing directory, but preserves archives with several top-level files.
fn package_root(staging: &Path) -> PathBuf {
    let children: Vec<_> = fs::read_dir(staging).into_iter().flatten().filter_map(Result::ok).collect();
    if children.len() == 1 && children[0].file_type().map(|t| t.is_dir()).unwrap_or(false) { children[0].path() } else { staging.to_path_buf() }
}

/// Scores likely application launchers. A score is deliberately explainable: archive metadata
/// and familiar launcher names beat arbitrary nested binaries, while library/document trees lose.
fn executable_candidates(root: &Path, archive_name: &str, policy: Policy) -> Result<Vec<LauncherCandidate>> {
    let folder_name = root.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
    let archive_name = archive_name.to_ascii_lowercase();
    let mut scores: BTreeMap<PathBuf, (i32, String)> = BTreeMap::new();

    // Desktop files express the package author's intended launcher, so they rank just below AppRun.
    for item in WalkDir::new(root).follow_links(false).into_iter() {
        let item = item?;
        if item.file_type().is_file() && item.path().extension().is_some_and(|ext| ext.eq_ignore_ascii_case("desktop")) {
            if let Some(target) = desktop_exec_target(item.path(), root)? {
                // A top-level desktop file is more likely to describe the package than one in a subcomponent.
                let proximity = item.path().strip_prefix(root)?.components().count().saturating_sub(1).min(10) as i32;
                add_candidate(&mut scores, root, &target, 95 - proximity, "desktop entry Exec target");
            }
        }
    }
    for item in WalkDir::new(root).follow_links(false).into_iter() {
        let item = item?; if !item.file_type().is_file() { continue; }
        let path = item.path();
        if !is_executable(path) { continue; }
        let filename = item.file_name().to_string_lossy().to_ascii_lowercase();
        let is_native_binary = security::is_elf(path);
        // Scripts are candidates only when their names clearly identify them as launchers.
        let is_launcher_script = filename.starts_with("start-") || filename.starts_with("launch-") || filename.starts_with("run-") || filename.ends_with(".sh");
        // An override admits any executable, e.g. a wrapper script without a launcher-like name.
        if !is_native_binary && !is_launcher_script && policy == Policy::Enforce { continue; }
        if filename == "apprun" && path.parent() == Some(root) { add_candidate(&mut scores, root, path, 100, "root AppRun"); continue; }
        let depth = path.strip_prefix(root)?.components().count();
        if is_launcher_script { add_candidate(&mut scores, root, path, 90, "named launcher script"); }
        if depth == 1 { add_candidate(&mut scores, root, path, 80, "executable at extraction root"); }
        if filename == archive_name { add_candidate(&mut scores, root, path, 70, "filename matches archive"); }
        if filename == folder_name { add_candidate(&mut scores, root, path, 70, "filename matches extraction folder"); }
        if is_native_binary { add_candidate(&mut scores, root, path, 20, "nested executable"); }
        else if policy == Policy::Override { add_candidate(&mut scores, root, path, 10, "executable script (safety checks overridden)"); }
    }
    let mut candidates: Vec<_> = scores.into_iter().map(|(relative_path, (score, reason))| LauncherCandidate { relative_path, score, reason }).collect();
    candidates.sort_by(|left, right| right.score.cmp(&left.score).then_with(|| left.relative_path.cmp(&right.relative_path)));
    Ok(candidates)
}

/// Records only the best reason for a path, then applies directory penalties to every heuristic.
fn add_candidate(scores: &mut BTreeMap<PathBuf, (i32, String)>, root: &Path, path: &Path, score: i32, reason: &str) {
    let Ok(relative) = path.strip_prefix(root) else { return; };
    let penalty = candidate_penalty(relative);
    let final_score = score + penalty;
    let entry = scores.entry(relative.to_path_buf()).or_insert((final_score, reason.to_owned()));
    if final_score > entry.0 { *entry = (final_score, reason.to_owned()); }
}

/// Penalizes trees conventionally used for dependencies, documentation, or development artifacts.
fn candidate_penalty(relative: &Path) -> i32 {
    let components: Vec<_> = relative.components().filter_map(|part| match part { Component::Normal(value) => value.to_str().map(|s| s.to_ascii_lowercase()), _ => None }).collect();
    let unsafe_tree = ["lib", "lib64", "share", "doc", "docs", "include", "node_modules", ".git"];
    if components.iter().any(|component| unsafe_tree.contains(&component.as_str())) { return -100; }
    // Tor transport binaries are helpers, not the browser's user-facing launcher.
    if components.windows(4).any(|path| path == ["browser", "torbrowser", "tor", "pluggabletransports"]) { return -100; }
    0
}

/// Returns whether a regular file has user execute permission; never follow a link while deciding.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::symlink_metadata(path).map(|metadata| metadata.file_type().is_file() && metadata.permissions().mode() & 0o100 != 0).unwrap_or(false) }
    #[cfg(not(unix))] { let _ = path; false }
}

/// Reads a desktop entry's command without executing it and resolves it relative to that file.
fn desktop_exec_target(desktop_file: &Path, root: &Path) -> Result<Option<PathBuf>> {
    let contents = match fs::read_to_string(desktop_file) { Ok(contents) => contents, Err(_) => return Ok(None) };
    let exec = contents.lines().find_map(|line| line.strip_prefix("Exec="));
    let Some(exec) = exec else { return Ok(None); };
    // Shell operators and field codes make the target ambiguous and are never acceptable here.
    if exec.is_empty() || exec.contains(['\n', '\r', '`', '$', ';', '|', '&', '<', '>']) { return Ok(None); }
    let Some(command) = first_exec_word(exec) else { return Ok(None); };
    if command.contains('%') { return Ok(None); }
    let command_path = Path::new(&command);
    let target = if command_path.is_absolute() { command_path.to_path_buf() } else { desktop_file.parent().unwrap_or(root).join(command_path) };
    if !target.starts_with(root) || !is_executable(&target) { return Ok(None); }
    Ok(Some(target))
}

/// Parses the first desktop Exec argument, supporting the quoted launcher paths common in bundles.
fn first_exec_word(exec: &str) -> Option<String> {
    let mut result = String::new(); let mut quoted = false; let mut escaped = false;
    for character in exec.chars() {
        if escaped { result.push(character); escaped = false; continue; }
        if character == '\\' { escaped = true; continue; }
        if character == '"' { quoted = !quoted; continue; }
        if character.is_whitespace() && !quoted { break; }
        result.push(character);
    }
    (!quoted && !escaped && !result.is_empty()).then_some(result)
}

/// Calculates a non-arbitrary final directory, optionally using a numbered sibling for Keep both.
fn destination_for(root: &Path, name: &str, choice: ExistingChoice) -> Result<PathBuf> {
    let first = utils::direct_child(root, name)?;
    if !first.exists() || choice != ExistingChoice::KeepBoth { return Ok(first); }
    for number in 2..10_000 { let candidate = utils::direct_child(root, &format!("{name} ({number})"))?; if !candidate.exists() { return Ok(candidate); } }
    bail!("could not find a free installation name")
}

/// Removes only the exact directory and desktop file recorded by TarDrop.
pub fn uninstall(app: &InstalledApp) -> Result<()> {
    let root = utils::applications_dir()?;
    if app.directory.parent() != Some(root.as_path()) { bail!("refusing to remove directory outside ~/Applications") }
    if app.desktop_file.parent() != Some(utils::applications_database_dir()?.as_path()) { bail!("refusing to remove desktop entry outside XDG applications") }
    if app.directory.exists() { fs::remove_dir_all(&app.directory)?; }
    if app.desktop_file.exists() { fs::remove_file(&app.desktop_file)?; }
    if let Some(icon) = &app.icon {
        let icon_root = dirs::data_local_dir().unwrap_or_default().join("icons").join("TarDrop");
        if icon.parent() == Some(icon_root.as_path()) && icon.exists() { fs::remove_file(icon)?; }
    }
    desktop::refresh_integrations();
    updates::record_removal(&app.directory).context("application files were removed, but database cleanup failed")?;
    Ok(())
}
