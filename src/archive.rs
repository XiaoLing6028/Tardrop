//! Archive recognition and extraction.
//!
//! Every archive member is checked before it is written.  This module never delegates
//! extraction to a shell command, which avoids command injection and inconsistent tools.

use std::{fs, io::{self, Read}, path::{Component, Path, PathBuf}};
use anyhow::{bail, Context, Result};
use crate::security::{Policy, Rejected};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use tar::{Archive, EntryType};
use xz2::read::XzDecoder;

/// Formats which TarDrop can safely read today. Add a variant and extractor to extend it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat { Tar, TarGz, TarXz, TarBz2, Zip }

/// Identifies a supported type by extension; no archive is extracted based on a guessed command.
pub fn detect(path: &Path) -> Result<ArchiveFormat> {
    let lower = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") { Ok(ArchiveFormat::TarGz) }
    else if lower.ends_with(".tar.xz") { Ok(ArchiveFormat::TarXz) }
    else if lower.ends_with(".tar.bz2") { Ok(ArchiveFormat::TarBz2) }
    else if lower.ends_with(".tar") { Ok(ArchiveFormat::Tar) }
    else if lower.ends_with(".zip") { Ok(ArchiveFormat::Zip) }
    else { bail!("Unsupported archive type. Use tar, tar.gz, tgz, tar.xz, tar.bz2, or zip.") }
}

/// Extracts `source` into the empty, private `destination` directory.
/// Symlinks, hardlinks, device files, and traversal paths are rejected rather than followed.
/// Under `Policy::Override` they are relaxed instead (see `Extractor`), but traversal paths are
/// still never written: that rule protects the rest of the home directory, not the application.
pub fn extract(source: &Path, format: ArchiveFormat, destination: &Path, policy: Policy, log: &mut Vec<String>) -> Result<()> {
    fs::create_dir_all(destination).context("could not create extraction directory")?;
    let mut extractor = Extractor { root: destination, policy, log, links: Vec::new() };
    match format {
        ArchiveFormat::Tar => extractor.tar(Archive::new(fs::File::open(source)?))?,
        ArchiveFormat::TarGz => extractor.tar(Archive::new(GzDecoder::new(fs::File::open(source)?)))?,
        ArchiveFormat::TarXz => extractor.tar(Archive::new(XzDecoder::new(fs::File::open(source)?)))?,
        ArchiveFormat::TarBz2 => extractor.tar(Archive::new(BzDecoder::new(fs::File::open(source)?)))?,
        ArchiveFormat::Zip => extractor.zip(source)?,
    }
    extractor.create_links()
}

/// Ensures an archive path is a relative normal path and turns it into a destination path.
fn safe_destination(root: &Path, archive_path: &Path) -> Result<PathBuf> {
    if archive_path.as_os_str().is_empty() { return Err(Rejected("archive contains an empty path".into()).into()) }
    let mut result = root.to_path_buf();
    for component in archive_path.components() {
        match component {
            Component::Normal(piece) if piece != "." => result.push(piece),
            _ => return Err(Rejected(format!("archive contains unsafe path: {}", archive_path.display())).into()),
        }
    }
    Ok(result)
}

/// Per-archive extraction state. Symbolic links allowed by an override are collected in `links`
/// and created only after every file is written, so no member can ever be written through a link.
struct Extractor<'a> { root: &'a Path, policy: Policy, log: &'a mut Vec<String>, links: Vec<(PathBuf, PathBuf)> }

impl Extractor<'_> {
    /// Refuses under `Enforce`; under `Override` records the relaxed check and lets extraction go on.
    fn relax(&mut self, reason: String) -> Result<()> {
        if self.policy == Policy::Enforce { return Err(Rejected(reason).into()); }
        self.log.push(format!("Safety check overridden: {reason}"));
        Ok(())
    }

    /// Resolves a member path; a traversal path is skipped under `Override`, never written.
    fn destination(&mut self, relative: &Path) -> Result<Option<PathBuf>> {
        match safe_destination(self.root, relative) {
            Ok(path) => Ok(Some(path)),
            Err(error) if self.policy == Policy::Override && error.is::<Rejected>() => { self.log.push(format!("Skipped member outside the install folder: {}", relative.display())); Ok(None) }
            Err(error) => Err(error),
        }
    }

    /// Opens a new regular file. A duplicate member replaces an earlier file only under `Override`;
    /// nothing but regular files and directories exist during this pass, so removal cannot escape.
    fn create(&mut self, output: &Path) -> Result<Option<fs::File>> {
        if let Some(parent) = output.parent() { fs::create_dir_all(parent)?; }
        if output.symlink_metadata().is_ok() {
            self.relax(format!("archive contains duplicate member: {}", output.strip_prefix(self.root).unwrap_or(output).display()))?;
            if !output.symlink_metadata()?.is_file() { self.log.push(format!("Skipped duplicate of a directory: {}", output.display())); return Ok(None); }
            fs::remove_file(output)?;
        }
        Ok(Some(fs::OpenOptions::new().write(true).create_new(true).open(output).with_context(|| format!("could not create {}", output.display()))?))
    }

    /// Streams tar data into regular files only. Streaming limits memory use for large archives.
    fn tar<R: Read>(&mut self, mut archive: Archive<R>) -> Result<()> {
        for item in archive.entries().context("invalid tar archive")? {
            let mut entry = item.context("could not read tar member")?;
            let relative = entry.path().context("invalid tar path")?.into_owned();
            let Some(output) = self.destination(&relative)? else { continue };
            let kind = entry.header().entry_type();
            if kind == EntryType::Directory {
                fs::create_dir_all(&output)?;
            } else if kind == EntryType::Regular || kind == EntryType::GNUSparse {
                let Some(mut file) = self.create(&output)? else { continue };
                io::copy(&mut entry, &mut file)?;
                set_safe_mode(&output, entry.header().mode().unwrap_or(0));
            } else if kind == EntryType::Symlink {
                self.relax(format!("archive contains a symbolic link: {}", relative.display()))?;
                let target = entry.link_name()?.ok_or_else(|| anyhow::anyhow!("symbolic link without a target: {}", relative.display()))?.into_owned();
                self.links.push((output, target));
            } else if kind == EntryType::Link {
                self.relax(format!("archive contains a hard link: {}", relative.display()))?;
                // Materialised as a copy of an earlier member, so it can never alias a file outside.
                let target = entry.link_name()?.ok_or_else(|| anyhow::anyhow!("hard link without a target: {}", relative.display()))?.into_owned();
                let source = self.destination(&target)?.filter(|source| source.symlink_metadata().is_ok_and(|metadata| metadata.is_file()));
                let Some(source) = source else { self.log.push(format!("Skipped hard link to a missing file: {}", relative.display())); continue };
                if self.create(&output)?.is_some() { fs::copy(&source, &output)?; }
            } else {
                self.relax(format!("archive contains unsupported special file: {}", relative.display()))?;
                self.log.push(format!("Skipped special file: {}", relative.display()));
            }
        }
        Ok(())
    }

    /// Extracts ZIP files with the same path and link policy as tar files.
    fn zip(&mut self, source: &Path) -> Result<()> {
        let file = fs::File::open(source)?;
        let mut archive = zip::ZipArchive::new(file).context("invalid zip archive")?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            // `enclosed_name` refuses traversal itself; the raw name is used only to report the skip.
            let Some(relative) = entry.enclosed_name() else {
                if self.policy == Policy::Enforce { return Err(Rejected(format!("archive contains unsafe path: {}", entry.name())).into()); }
                self.log.push(format!("Skipped member outside the install folder: {}", entry.name()));
                continue;
            };
            let Some(output) = self.destination(&relative)? else { continue };
            let mode = entry.unix_mode().unwrap_or(0o644);
            if (mode & 0o170000) == 0o120000 {
                self.relax(format!("archive contains a symbolic link: {}", relative.display()))?;
                let mut target = String::new(); entry.read_to_string(&mut target)?;
                self.links.push((output, PathBuf::from(target)));
            } else if entry.is_dir() {
                fs::create_dir_all(&output)?;
            } else {
                let Some(mut file) = self.create(&output)? else { continue };
                io::copy(&mut entry, &mut file)?;
                set_safe_mode(&output, mode);
            }
        }
        Ok(())
    }

    /// Creates overridden symbolic links last. A link is placed only where no ancestor inside the
    /// staging root is itself a link, so an earlier link cannot redirect a later one outside.
    fn create_links(&mut self) -> Result<()> {
        for (output, target) in std::mem::take(&mut self.links) {
            let relative = output.strip_prefix(self.root)?.to_path_buf();
            let mut prefix = self.root.to_path_buf();
            let linked = relative.parent().into_iter().flat_map(Path::components).any(|part| { prefix.push(part); prefix.symlink_metadata().is_ok_and(|metadata| metadata.is_symlink()) });
            if linked || output.symlink_metadata().is_ok() { self.log.push(format!("Skipped symbolic link that would pass through another link or overwrite a member: {}", relative.display())); continue; }
            if let Some(parent) = output.parent() { fs::create_dir_all(parent)?; }
            #[cfg(unix)] std::os::unix::fs::symlink(&target, &output).with_context(|| format!("could not create symbolic link {}", relative.display()))?;
        }
        Ok(())
    }
}

/// Retains only ordinary read/write/execute bits. This prevents setuid/setgid archives.
fn set_safe_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777)); }
}
