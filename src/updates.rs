//! Persistent installed-application records and pluggable update providers.
//!
//! This module does not participate in extraction. It records completed installer transactions
//! and, when a user explicitly requests it, coordinates a reversible replacement transaction.

use std::{collections::BTreeMap, fs, path::{Path, PathBuf}, time::{SystemTime, UNIX_EPOCH}};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tempfile::Builder;
use crate::{installer::{self, ExistingChoice, InstallResult, InstalledApp}, security::Policy, utils};

/// One installed portable application, stored independently from the application archive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledRecord {
    pub id: String, pub name: String, pub version: Option<String>, pub install_path: PathBuf,
    pub desktop_file_path: PathBuf, pub icon_path: Option<PathBuf>, pub source_url: Option<String>,
    pub archive_filename: String, pub install_date: u64, pub last_update_check: Option<u64>,
    pub latest_version: Option<String>, pub update_provider: ProviderKind, pub custom_metadata: BTreeMap<String, String>,
}

/// Provider selection persisted in a readable, forward-compatible form.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind { GitHubReleases, StaticUrl, WebsiteScraper, #[default] Manual }

/// Global update preferences stored beside the installed-app database.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateSettings { pub check_automatically: bool, pub notify_beta_releases: bool, pub check_on_startup: bool, pub interval: UpdateInterval }

impl Default for UpdateSettings {
    fn default() -> Self { Self { check_automatically: false, notify_beta_releases: false, check_on_startup: true, interval: UpdateInterval::Weekly } }
}

/// Frequency used to decide whether a start-up check is due.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateInterval { Daily, #[default] Weekly, Monthly, Never }

/// Result returned by a provider without exposing provider-specific network details to the UI.
#[derive(Clone, Debug)]
pub struct ReleaseInfo { pub version: String, pub download_url: Option<String>, pub notes: Option<String> }

/// A provider can inspect remote metadata and, when available, fetch the selected release archive.
pub trait UpdateProvider: Send + Sync {
    /// Retrieves current release metadata without changing the installed application.
    fn check_latest(&self) -> Result<ReleaseInfo>;
    /// Downloads the provider's latest release to a private temporary archive.
    fn download_latest(&self, destination: &Path) -> Result<ReleaseInfo>;
}

/// Persists installation records using a small JSON document which users can inspect and back up.
pub struct InstalledDatabase;

impl InstalledDatabase {
    /// Returns all records, treating a missing database as an empty first-run state.
    pub fn load() -> Result<Vec<InstalledRecord>> {
        let path = database_path()?;
        if !path.exists() { return Ok(Vec::new()); }
        let text = fs::read_to_string(&path).context("could not read installed applications database")?;
        serde_json::from_str(&text).context("installed applications database is invalid")
    }

    /// Atomically saves records so a crash cannot leave a half-written JSON database.
    pub fn save(records: &[InstalledRecord]) -> Result<()> {
        let path = database_path()?; let parent = path.parent().expect("database path has parent");
        fs::create_dir_all(parent)?;
        let temporary = path.with_extension("json.new");
        fs::write(&temporary, serde_json::to_vec_pretty(records)?)?;
        fs::rename(temporary, path)?; Ok(())
    }

    /// Inserts or replaces a record by desktop path, which is stable across same-app updates.
    pub fn upsert(record: InstalledRecord) -> Result<()> {
        let mut records = Self::load()?;
        if let Some(existing) = records.iter_mut().find(|existing| existing.desktop_file_path == record.desktop_file_path || existing.install_path == record.install_path) { *existing = record; }
        else { records.push(record); }
        Self::save(&records)
    }

    /// Removes only a record belonging to the exact managed installation path.
    pub fn remove(install_path: &Path) -> Result<()> {
        let mut records = Self::load()?; records.retain(|record| record.install_path != install_path); Self::save(&records)
    }

    /// Reads settings, returning safe defaults before the user has changed anything.
    pub fn load_settings() -> Result<UpdateSettings> {
        let path = settings_path()?; if !path.exists() { return Ok(UpdateSettings::default()); }
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }

    /// Saves settings atomically using the same directory as the installed-app database.
    pub fn save_settings(settings: &UpdateSettings) -> Result<()> {
        let path = settings_path()?; fs::create_dir_all(path.parent().expect("settings path has parent"))?;
        fs::write(&path, serde_json::to_vec_pretty(settings)?)?; Ok(())
    }
}

/// Records an installer success. Unknown sources deliberately default to Manual, never guessed.
pub fn record_install(app: &InstalledApp, archive: &Path) -> Result<()> {
    let existing = InstalledDatabase::load()?.into_iter().find(|record| record.install_path == app.directory);
    let record = InstalledRecord {
        id: utils::desktop_id(&app.name), name: app.name.clone(), version: detect_version(app, archive),
        install_path: app.directory.clone(), desktop_file_path: app.desktop_file.clone(), icon_path: app.icon.clone(),
        source_url: existing.as_ref().and_then(|record| record.source_url.clone()), archive_filename: archive.file_name().and_then(|name| name.to_str()).unwrap_or("archive").to_owned(),
        install_date: existing.map(|record| record.install_date).unwrap_or_else(now), last_update_check: None,
        latest_version: None, update_provider: ProviderKind::Manual, custom_metadata: BTreeMap::new(),
    };
    InstalledDatabase::upsert(record)
}

/// Removes the database record after the installer has removed its owned files.
pub fn record_removal(install_path: &Path) -> Result<()> { InstalledDatabase::remove(install_path) }

/// Checks a record through its configured provider and persists the resulting timestamp/version.
pub fn check_for_update(record: &mut InstalledRecord) -> Result<Option<ReleaseInfo>> {
    let provider = provider_for(record)?;
    let release = provider.check_latest()?;
    record.last_update_check = Some(now()); record.latest_version = Some(release.version.clone());
    InstalledDatabase::upsert(record.clone())?;
    Ok(is_newer(record.version.as_deref(), &release.version).then_some(release))
}

/// Replaces an application only after a newly downloaded archive has passed the existing installer.
/// The prior directory and launcher bytes remain in a private backup until the new install succeeds.
pub fn update(record: &InstalledRecord, log: &mut Vec<String>) -> Result<InstalledRecord> {
    let provider = provider_for(record)?;
    let temporary = tempfile::tempdir().context("could not create update download directory")?;
    // Keep the remote filename extension: the existing installer deliberately detects formats by it.
    let announced = provider.check_latest()?;
    let archive = temporary.path().join(download_filename(announced.download_url.as_deref()));
    log.push("Downloading update…".into());
    let release = provider.download_latest(&archive)?;
    let parent = record.install_path.parent().ok_or_else(|| anyhow::anyhow!("invalid installation path"))?;
    let backup = Builder::new().prefix(".tardrop-update-backup-").tempdir_in(parent)?;
    let saved_directory = backup.path().join("application");
    let old_desktop = fs::read(&record.desktop_file_path).ok();
    let old_icon = record.icon_path.as_ref().and_then(|path| fs::read(path).ok());
    let old_permissions = fs::metadata(&record.install_path).ok().map(|metadata| metadata.permissions());
    log.push("Creating rollback snapshot…".into());
    fs::rename(&record.install_path, &saved_directory).context("could not move current application into rollback snapshot")?;
    let result = installer::install(&archive, ExistingChoice::Replace, None, Policy::Enforce, log);
    let installed = match result {
        Ok(InstallResult::Installed(app)) if app.directory == record.install_path => app,
        Ok(InstallResult::Installed(app)) => {
            // A changed application identity must never overwrite a different managed directory.
            if app.directory.exists() { fs::remove_dir_all(&app.directory)?; }
            let _ = InstalledDatabase::remove(&app.directory);
            rollback(record, &saved_directory, old_desktop.as_deref(), old_icon.as_deref())?;
            bail!("downloaded archive identifies as a different application; installation was rolled back")
        }
        Ok(InstallResult::NeedsLauncherChoice(_)) => { rollback(record, &saved_directory, old_desktop.as_deref(), old_icon.as_deref())?; bail!("update needs a launcher choice; installation was rolled back") }
        Err(error) => { rollback(record, &saved_directory, old_desktop.as_deref(), old_icon.as_deref())?; return Err(error.context("update failed and was rolled back")); }
    };
    // Preserve the established launcher, icon and root permissions: user customisations survive.
    if let Some(desktop) = old_desktop { fs::write(&record.desktop_file_path, desktop)?; }
    if let (Some(path), Some(icon)) = (&record.icon_path, old_icon) { fs::write(path, icon)?; }
    if let Some(permissions) = old_permissions { fs::set_permissions(&installed.directory, permissions)?; }
    let mut updated = record.clone(); updated.version = Some(release.version); updated.archive_filename = record.archive_filename.clone(); updated.last_update_check = Some(now()); updated.latest_version = None;
    InstalledDatabase::upsert(updated.clone())?;
    log.push("Update completed and rollback snapshot discarded.".into()); Ok(updated)
}

/// Restores every moved user-owned file after a failed update attempt.
fn rollback(record: &InstalledRecord, saved_directory: &Path, desktop: Option<&[u8]>, icon: Option<&[u8]>) -> Result<()> {
    if record.install_path.exists() { fs::remove_dir_all(&record.install_path)?; }
    fs::rename(saved_directory, &record.install_path)?;
    if let Some(desktop) = desktop { fs::write(&record.desktop_file_path, desktop)?; }
    if let (Some(path), Some(icon)) = (&record.icon_path, icon) { fs::write(path, icon)?; }
    Ok(())
}

/// Detects versions from non-executing metadata and names. Executing `--version` would violate
/// TarDrop's rule that archives are never run automatically, even after installation.
fn detect_version(app: &InstalledApp, archive: &Path) -> Option<String> {
    // Vendors commonly add one of these extension keys to their desktop entry. We intentionally
    // ignore the freedesktop `Version=1.5` format key because it is not the app version.
    if let Ok(entry) = fs::read_to_string(&app.desktop_file) {
        for key in ["X-AppVersion=", "X-Version=", "AppVersion="] {
            if let Some(value) = entry.lines().find_map(|line| line.strip_prefix(key)).map(str::trim).filter(|value| !value.is_empty() && value.len() < 80) { return Some(value.to_owned()); }
        }
    }
    for path in [app.directory.join("VERSION"), app.directory.join("version"), app.directory.join("VERSION.txt")] {
        if let Ok(value) = fs::read_to_string(path) { let value = value.lines().next().unwrap_or("").trim(); if !value.is_empty() && value.len() < 80 { return Some(value.to_owned()); } }
    }
    version_from_text(&archive.file_name()?.to_string_lossy())
}

/// Finds a conventional dotted version segment in release filenames without guessing arbitrary text.
fn version_from_text(text: &str) -> Option<String> {
    text.split(|character: char| !(character.is_ascii_alphanumeric() || character == '.')).find(|part| part.chars().filter(|character| character.is_ascii_digit()).count() >= 2 && part.contains('.')).map(|part| part.trim_start_matches('v').to_owned())
}

/// Creates an appropriate provider from persisted data; missing source information stays manual.
fn provider_for(record: &InstalledRecord) -> Result<Box<dyn UpdateProvider>> {
    match record.update_provider {
        ProviderKind::GitHubReleases => Ok(Box::new(GitHubReleasesProvider::new(record.source_url.as_deref().ok_or_else(|| anyhow::anyhow!("GitHub provider needs a repository URL"))?)?)),
        ProviderKind::StaticUrl => Ok(Box::new(StaticUrlProvider::from_metadata(record)?)),
        ProviderKind::WebsiteScraper => Ok(Box::new(WebsiteScraperProvider::from_metadata(record)?)),
        ProviderKind::Manual => Ok(Box::new(ManualProvider)),
    }
}

/// GitHub's public releases API provider, selected explicitly through database metadata.
struct GitHubReleasesProvider { owner: String, repository: String }
impl GitHubReleasesProvider {
    fn new(url: &str) -> Result<Self> {
        let parts: Vec<_> = url.trim_end_matches('/').split('/').collect();
        if parts.len() < 2 { bail!("invalid GitHub repository URL") }
        Ok(Self { owner: parts[parts.len() - 2].to_owned(), repository: parts[parts.len() - 1].to_owned() })
    }
    fn latest(&self) -> Result<GitHubRelease> {
        let url = format!("https://api.github.com/repos/{}/{}/releases/latest", self.owner, self.repository);
        reqwest::blocking::Client::new().get(url).header("User-Agent", "TarDrop").send()?.error_for_status()?.json().context("invalid GitHub release response")
    }
}
impl UpdateProvider for GitHubReleasesProvider {
    fn check_latest(&self) -> Result<ReleaseInfo> { let release = self.latest()?; Ok(release.info()) }
    fn download_latest(&self, destination: &Path) -> Result<ReleaseInfo> { let release = self.latest()?; let asset = release.assets.first().ok_or_else(|| anyhow::anyhow!("GitHub release has no downloadable assets"))?; download(&asset.browser_download_url, destination)?; Ok(release.info()) }
}

/// A static provider reads its URL and version from custom metadata, useful for stable vendors.
struct StaticUrlProvider { url: String, version: String, notes: Option<String> }
impl StaticUrlProvider { fn from_metadata(record: &InstalledRecord) -> Result<Self> { Ok(Self { url: record.custom_metadata.get("static_download_url").cloned().ok_or_else(|| anyhow::anyhow!("static provider needs static_download_url metadata"))?, version: record.custom_metadata.get("static_version").cloned().ok_or_else(|| anyhow::anyhow!("static provider needs static_version metadata"))?, notes: record.custom_metadata.get("release_notes").cloned() }) } }
impl UpdateProvider for StaticUrlProvider { fn check_latest(&self) -> Result<ReleaseInfo> { Ok(ReleaseInfo { version: self.version.clone(), download_url: Some(self.url.clone()), notes: self.notes.clone() }) } fn download_latest(&self, destination: &Path) -> Result<ReleaseInfo> { download(&self.url, destination)?; self.check_latest() } }

/// A deliberately narrow website provider for vendors with a stable plain-text version endpoint.
/// It avoids executing page JavaScript or evaluating arbitrary scraping expressions.
struct WebsiteScraperProvider { version_url: String, download_url: String, version_prefix: Option<String>, notes_url: Option<String> }
impl WebsiteScraperProvider {
    fn from_metadata(record: &InstalledRecord) -> Result<Self> {
        let metadata = &record.custom_metadata;
        Ok(Self {
            version_url: metadata.get("website_version_url").cloned().ok_or_else(|| anyhow::anyhow!("website provider needs website_version_url metadata"))?,
            download_url: metadata.get("website_download_url").cloned().ok_or_else(|| anyhow::anyhow!("website provider needs website_download_url metadata"))?,
            version_prefix: metadata.get("website_version_prefix").cloned(), notes_url: metadata.get("website_notes_url").cloned(),
        })
    }
}
impl UpdateProvider for WebsiteScraperProvider {
    fn check_latest(&self) -> Result<ReleaseInfo> {
        let text = reqwest::blocking::Client::new().get(&self.version_url).header("User-Agent", "TarDrop").send()?.error_for_status()?.text()?;
        let candidate = match &self.version_prefix { Some(prefix) => text.lines().find_map(|line| line.trim().strip_prefix(prefix).map(str::trim)), None => text.lines().map(str::trim).find(|line| !line.is_empty()) }.ok_or_else(|| anyhow::anyhow!("website version endpoint did not contain a version"))?;
        if candidate.len() > 80 || !candidate.chars().any(|character| character.is_ascii_digit()) { bail!("website version endpoint returned an invalid version") }
        let notes = self.notes_url.as_ref().and_then(|url| reqwest::blocking::get(url).ok()?.error_for_status().ok()?.text().ok());
        Ok(ReleaseInfo { version: candidate.trim_start_matches('v').to_owned(), download_url: Some(self.download_url.clone()), notes })
    }
    fn download_latest(&self, destination: &Path) -> Result<ReleaseInfo> { download(&self.download_url, destination)?; self.check_latest() }
}
struct ManualProvider;
impl UpdateProvider for ManualProvider { fn check_latest(&self) -> Result<ReleaseInfo> { bail!("this application has no update source configured") } fn download_latest(&self, _: &Path) -> Result<ReleaseInfo> { bail!("this application has no update source configured") } }

/// Downloads through HTTPS into a private path; installer validation still verifies the archive.
fn download(url: &str, destination: &Path) -> Result<()> { let mut response = reqwest::blocking::Client::new().get(url).header("User-Agent", "TarDrop").send()?.error_for_status()?; let mut file = fs::File::create(destination)?; std::io::copy(&mut response, &mut file)?; Ok(()) }

/// Uses only a basename from the provider URL and falls back to `.tar` for safe installer routing.
fn download_filename(url: Option<&str>) -> String {
    url.and_then(|url| url.split('?').next()).and_then(|url| url.rsplit('/').next()).filter(|name| crate::archive::detect(Path::new(name)).is_ok()).map(str::to_owned).unwrap_or_else(|| "update.tar".to_owned())
}

#[derive(Deserialize)]
struct GitHubRelease { tag_name: String, body: Option<String>, assets: Vec<GitHubAsset> }
impl GitHubRelease { fn info(&self) -> ReleaseInfo { ReleaseInfo { version: self.tag_name.trim_start_matches('v').to_owned(), download_url: self.assets.first().map(|asset| asset.browser_download_url.clone()), notes: self.body.clone() } } }
#[derive(Deserialize)] struct GitHubAsset { browser_download_url: String }

/// Compares dot-separated numeric releases conservatively; non-numeric tags are only different.
fn is_newer(installed: Option<&str>, latest: &str) -> bool { installed.map(|old| version_parts(latest) > version_parts(old)).unwrap_or(true) }
fn version_parts(value: &str) -> Vec<u64> { value.trim_start_matches('v').split('.').map(|part| part.parse().unwrap_or(0)).collect() }
fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }
fn data_dir() -> Result<PathBuf> { Ok(dirs::data_local_dir().ok_or_else(|| anyhow::anyhow!("could not determine XDG data directory"))?.join("tardrop")) }
fn database_path() -> Result<PathBuf> { Ok(data_dir()?.join("installed-apps.json")) }
fn settings_path() -> Result<PathBuf> { Ok(data_dir()?.join("settings.json")) }
