//! Archive recognition and extraction.
//!
//! Every archive member is checked before it is written.  This module never delegates
//! extraction to a shell command, which avoids command injection and inconsistent tools.

use std::{collections::{BTreeSet, HashMap, VecDeque}, ffi::OsString, fs, io::{self, Read}, path::{Component, Path, PathBuf}};
use anyhow::{bail, Context, Result};
use crate::security::{Findings, LinkFinding, LinkKind, Policy, Rejected};
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

/// Reads every member header without writing anything, so a refusal can show the whole archive's
/// links and installer scripts rather than only the first member `extract` would stop at.
pub fn survey(source: &Path, format: ArchiveFormat) -> Result<Findings> {
    let mut members = Members::default();
    match format {
        ArchiveFormat::Tar => members.tar(Archive::new(fs::File::open(source)?))?,
        ArchiveFormat::TarGz => members.tar(Archive::new(GzDecoder::new(fs::File::open(source)?)))?,
        ArchiveFormat::TarXz => members.tar(Archive::new(XzDecoder::new(fs::File::open(source)?)))?,
        ArchiveFormat::TarBz2 => members.tar(Archive::new(BzDecoder::new(fs::File::open(source)?)))?,
        ArchiveFormat::Zip => members.zip(source)?,
    }
    Ok(members.findings())
}

/// What `survey` needs to know about one member; special files are left to `extract` to refuse.
enum Kind { Directory, File, Link(PathBuf, LinkKind), Special }

/// Facts gathered by `survey`. The top-level names decide which directory becomes the published
/// package, mirroring `installer::package_root`, because a link is judged against that folder.
#[derive(Default)]
struct Members { links: Vec<(PathBuf, PathBuf, LinkKind)>, scripts: Vec<PathBuf>, tops: BTreeSet<OsString>, directories: BTreeSet<OsString> }

impl Members {
    /// Records one member. Paths `extract` would never write cannot affect the installed package.
    fn note(&mut self, path: &Path, kind: Kind) {
        let Ok(relative) = safe_destination(Path::new(""), path) else { return };
        if matches!(kind, Kind::Special) { return; }
        let mut parts = relative.components();
        let Some(top) = parts.next().map(|part| part.as_os_str().to_owned()) else { return };
        if parts.next().is_some() || matches!(kind, Kind::Directory) { self.directories.insert(top.clone()); }
        self.tops.insert(top);
        let script = relative.file_name().is_some_and(|name| name.eq_ignore_ascii_case("install.sh"));
        match kind {
            Kind::Link(target, link) => { if script { self.scripts.push(relative.clone()); } self.links.push((relative, target, link)); }
            Kind::File if script => self.scripts.push(relative),
            _ => {}
        }
    }

    /// Classifies tar members from their headers; only link names are read, never file data.
    fn tar<R: Read>(&mut self, mut archive: Archive<R>) -> Result<()> {
        for item in archive.entries().context("invalid tar archive")? {
            let entry = item.context("could not read tar member")?;
            let path = entry.path().context("invalid tar path")?.into_owned();
            let target = || -> Result<PathBuf> { Ok(entry.link_name()?.map(|name| name.into_owned()).unwrap_or_default()) };
            let kind = match entry.header().entry_type() {
                EntryType::Directory => Kind::Directory,
                EntryType::Regular | EntryType::GNUSparse => Kind::File,
                EntryType::Symlink => Kind::Link(target()?, LinkKind::Symbolic),
                EntryType::Link => Kind::Link(target()?, LinkKind::Hard),
                _ => Kind::Special,
            };
            self.note(&path, kind);
        }
        Ok(())
    }

    /// Classifies ZIP members with the same symlink test as `Extractor::zip`.
    fn zip(&mut self, source: &Path) -> Result<()> {
        let mut archive = zip::ZipArchive::new(fs::File::open(source)?).context("invalid zip archive")?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let Some(path) = entry.enclosed_name() else { continue };
            let kind = if (entry.unix_mode().unwrap_or(0o644) & 0o170000) == 0o120000 {
                // A ZIP link target is the entry's data; PATH_MAX bounds it so a "link" cannot be a bomb.
                let mut target = Vec::new(); entry.by_ref().take(4096).read_to_end(&mut target)?;
                Kind::Link(PathBuf::from(String::from_utf8_lossy(&target).into_owned()), LinkKind::Symbolic)
            } else if entry.is_dir() { Kind::Directory } else { Kind::File };
            self.note(&path, kind);
        }
        Ok(())
    }

    /// Settles the package folder, then judges every link against it.
    fn findings(self) -> Findings {
        let root = match self.tops.first() { Some(top) if self.tops.len() == 1 && self.directories.contains(top) => PathBuf::from(top), _ => PathBuf::new() };
        // The first link at a path is the one `extract` creates; later duplicates are skipped there.
        let mut symlinks = HashMap::new();
        for (path, target, kind) in &self.links { if *kind == LinkKind::Symbolic { symlinks.entry(path.clone()).or_insert_with(|| target.clone()); } }
        let links = self.links.into_iter().map(|(path, target, kind)| {
            // A symbolic link resolves from its own folder; a hard link names a member from the top.
            let base = match kind { LinkKind::Symbolic => path.parent().map(Path::to_path_buf).unwrap_or_default(), LinkKind::Hard => PathBuf::new() };
            let escapes = escapes(&root, &base, &target, &symlinks);
            LinkFinding { path, target, kind, escapes }
        }).collect();
        Findings { links, install_scripts: self.scripts }
    }
}

/// One step of a path walk: up to the parent, or into a named child.
enum Step { Up, Into(OsString) }

/// Splits a path into walk steps; `None` for an absolute path, which always leaves the package.
fn steps(path: &Path) -> Option<Vec<Step>> {
    let mut steps = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => steps.push(Step::Into(name.to_owned())),
            Component::ParentDir => steps.push(Step::Up),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(steps)
}

/// Walks `target` from `base` through the archive's own symbolic links, as the kernel will once
/// they exist, and reports whether the walk ever leaves `root`. A lexical check alone is not enough:
/// `a -> .` and `b -> a/..` each stay inside, but together `b` climbs out of the package.
fn escapes(root: &Path, base: &Path, target: &Path, symlinks: &HashMap<PathBuf, PathBuf>) -> bool {
    let (Some(base), Some(target)) = (steps(base), steps(target)) else { return true };
    let mut pending: VecDeque<Step> = base.into_iter().chain(target).collect();
    let mut position = PathBuf::new();
    let mut hops = 0;
    while let Some(step) = pending.pop_front() {
        match step {
            Step::Up => if !position.pop() || !position.starts_with(root) { return true },
            Step::Into(name) => {
                position.push(name);
                let Some(next) = symlinks.get(&position) else { continue };
                // Linux gives up after 40 links (ELOOP); a longer chain resolves nowhere at all.
                hops += 1; if hops > 40 { return false; }
                position.pop();
                let Some(next) = steps(next) else { return true };
                for step in next.into_iter().rev() { pending.push_front(step); }
            }
        }
    }
    !position.starts_with(root)
}

/// Extracts `source` into the empty, private `destination` directory.
/// Symlinks, hardlinks, device files, and traversal paths are rejected rather than followed.
/// Under `Policy::Override` they are relaxed instead (see `Extractor`), but traversal paths are
/// still never written: that rule protects the rest of the home directory, not the application.
/// `installer::install` surveys links before calling this; the link refusals here remain as the
/// backstop for an archive that changed between the two reads.
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
    if archive_path.as_os_str().is_empty() { return Err(Rejected::new("archive contains an empty path").into()) }
    let mut result = root.to_path_buf();
    for component in archive_path.components() {
        match component {
            Component::Normal(piece) if piece != "." => result.push(piece),
            _ => return Err(Rejected::new(format!("archive contains unsafe path: {}", archive_path.display())).into()),
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
        if self.policy == Policy::Enforce { return Err(Rejected::new(reason).into()); }
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
                if self.policy == Policy::Enforce { return Err(Rejected::new(format!("archive contains unsafe path: {}", entry.name())).into()); }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, Member::*, FAKE_ELF};

    /// Surveys a tar built from `members` and returns each link's path with its escape verdict.
    fn escape_verdicts(members: &[test_support::Member]) -> Vec<(String, bool)> {
        let directory = tempfile::tempdir().unwrap();
        let archive = test_support::tar(directory.path(), "app.tar", members);
        survey(&archive, ArchiveFormat::Tar).unwrap().links.into_iter().map(|link| (link.path.display().to_string(), link.escapes)).collect()
    }

    #[test]
    fn survey_reports_every_link_and_install_sh_not_just_the_first() {
        let directory = tempfile::tempdir().unwrap();
        let archive = test_support::tar(directory.path(), "app.tar", &[
            Dir("App"), File("App/app", FAKE_ELF, 0o755), File("App/install.sh", b"#!/bin/sh\n", 0o755),
            File("App/lib/libfoo.so.1", b"lib", 0o644), Symlink("App/lib/libfoo.so", "libfoo.so.1"),
            Symlink("App/share", "/usr/share"), Hardlink("App/app-copy", "App/app"),
        ]);
        let findings = survey(&archive, ArchiveFormat::Tar).unwrap();
        assert_eq!(findings.install_scripts, [PathBuf::from("App/install.sh")]);
        let links: Vec<_> = findings.links.iter().map(|link| (link.path.to_str().unwrap(), link.kind, link.escapes)).collect();
        assert_eq!(links, [("App/lib/libfoo.so", LinkKind::Symbolic, false), ("App/share", LinkKind::Symbolic, true), ("App/app-copy", LinkKind::Hard, false)]);
    }

    #[test]
    fn survey_of_a_clean_archive_finds_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let archive = test_support::tar(directory.path(), "app.tar", &[Dir("App"), File("App/app", FAKE_ELF, 0o755), File("App/README", b"hi", 0o644)]);
        assert!(!survey(&archive, ArchiveFormat::Tar).unwrap().blocks());
    }

    #[test]
    fn install_sh_is_found_at_any_depth_and_case() {
        let directory = tempfile::tempdir().unwrap();
        let archive = test_support::tar(directory.path(), "app.tar", &[File("App/app", FAKE_ELF, 0o755), File("App/tools/INSTALL.SH", b"#!/bin/sh\n", 0o644), File("App/install.sh.txt", b"notes", 0o644)]);
        assert_eq!(survey(&archive, ArchiveFormat::Tar).unwrap().install_scripts, [PathBuf::from("App/tools/INSTALL.SH")]);
    }

    #[test]
    fn symlink_climbing_out_of_the_package_root_escapes() {
        let verdicts = escape_verdicts(&[
            File("App/app", FAKE_ELF, 0o755),
            Symlink("App/inside", "lib/../app"), Symlink("App/deep/up", "../app"),
            Symlink("App/parent", "../../etc/passwd"), Symlink("App/absolute", "/etc/passwd"),
        ]);
        assert_eq!(verdicts, [("App/inside".into(), false), ("App/deep/up".into(), false), ("App/parent".into(), true), ("App/absolute".into(), true)]);
    }

    #[test]
    fn symlink_to_a_sibling_of_the_single_top_folder_escapes() {
        // `App` becomes ~/Applications/<Name>, so `../Other` would point at a different application.
        assert_eq!(escape_verdicts(&[File("App/app", FAKE_ELF, 0o755), Symlink("App/peer", "../App/app")]), [("App/peer".into(), true)]);
    }

    #[test]
    fn several_top_level_entries_make_the_archive_root_the_package() {
        assert_eq!(escape_verdicts(&[File("app", FAKE_ELF, 0o755), File("lib/x", b"x", 0o644), Symlink("lib/y", "../app")]), [("lib/y".into(), false)]);
    }

    #[test]
    fn symlink_chain_that_escapes_together_is_caught() {
        // Each target looks harmless on its own; `b` resolves through `a` to the package's parent.
        assert_eq!(escape_verdicts(&[File("App/app", FAKE_ELF, 0o755), Symlink("App/a", "."), Symlink("App/b", "a/..")]), [("App/a".into(), false), ("App/b".into(), true)]);
    }

    #[test]
    fn symlink_loop_stays_inside() {
        assert_eq!(escape_verdicts(&[File("App/app", FAKE_ELF, 0o755), Symlink("App/a", "b"), Symlink("App/b", "a")]), [("App/a".into(), false), ("App/b".into(), false)]);
    }

    #[test]
    fn hardlink_to_a_member_outside_the_package_escapes() {
        assert_eq!(escape_verdicts(&[File("App/app", FAKE_ELF, 0o755), Hardlink("App/copy", "App/app"), Hardlink("App/out", "../outside")]), [("App/copy".into(), false), ("App/out".into(), true)]);
    }

    #[test]
    fn zip_symlinks_are_surveyed_too() {
        let directory = tempfile::tempdir().unwrap();
        let archive = test_support::zip(directory.path(), "app.zip", &[File("App/app", FAKE_ELF, 0o755), Symlink("App/inside", "app"), Symlink("App/outside", "../../.ssh")]);
        let verdicts: Vec<_> = survey(&archive, ArchiveFormat::Zip).unwrap().links.into_iter().map(|link| (link.path.display().to_string(), link.escapes)).collect();
        assert_eq!(verdicts, [("App/inside".into(), false), ("App/outside".into(), true)]);
    }

    #[test]
    fn enforce_extraction_still_refuses_links() {
        let directory = tempfile::tempdir().unwrap();
        let archive = test_support::tar(directory.path(), "app.tar", &[File("App/app", FAKE_ELF, 0o755), Symlink("App/lib", "app")]);
        let staging = directory.path().join("staging");
        let error = extract(&archive, ArchiveFormat::Tar, &staging, Policy::Enforce, &mut Vec::new()).unwrap_err();
        assert!(error.downcast_ref::<Rejected>().is_some_and(|rejected| rejected.risk.is_none()), "{error:#}");
        assert!(staging.join("App/lib").symlink_metadata().is_err());
    }

    #[test]
    fn override_extraction_creates_links_last() {
        let directory = tempfile::tempdir().unwrap();
        let archive = test_support::tar(directory.path(), "app.tar", &[File("App/app", FAKE_ELF, 0o755), Symlink("App/run", "app"), Hardlink("App/copy", "App/app")]);
        let staging = directory.path().join("staging");
        let mut log = Vec::new();
        extract(&archive, ArchiveFormat::Tar, &staging, Policy::Override, &mut log).unwrap();
        assert_eq!(fs::read_link(staging.join("App/run")).unwrap(), Path::new("app"));
        // A hard link is materialised as an independent copy, never as an alias.
        assert!(staging.join("App/copy").symlink_metadata().unwrap().is_file());
        assert_eq!(fs::read(staging.join("App/copy")).unwrap(), FAKE_ELF);
        assert!(log.iter().any(|line| line.starts_with("Safety check overridden")));
    }
}
