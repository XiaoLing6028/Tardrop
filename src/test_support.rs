//! Unit-test helpers: a sandboxed HOME, and builders for the archives tests feed to the installer.
//! Archives are written with the same `tar` and `zip` crates the extractor reads, so a test
//! exercises the production parsing path.

use std::{fs, path::{Path, PathBuf}, process::Command};
use tar::{Builder, EntryType, Header};

/// Runs the calling test's `body` with HOME pointing at a fresh directory, so installs never reach
/// the real `~/Applications`. The test re-executes itself as a child process, found by the name
/// libtest gives the test's thread: changing HOME in-process needs `std::env::set_var`, which is
/// `unsafe` in edition 2024 and forbidden in this crate. `--include-ignored` lets the child run a
/// test that is ignored by default, such as one needing a display.
pub fn with_sandbox_home(body: impl FnOnce(&Path)) {
    const MARKER: &str = "TARDROP_TEST_HOME";
    if let Some(home) = std::env::var_os(MARKER) { return body(Path::new(&home)); }
    let name = std::thread::current().name().expect("libtest names each test's thread").to_owned();
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([name.as_str(), "--exact", "--nocapture", "--test-threads=1", "--include-ignored"])
        .env(MARKER, home.path()).env("HOME", home.path())
        .env_remove("XDG_DATA_HOME").env_remove("XDG_CONFIG_HOME").env_remove("XDG_CACHE_HOME")
        // `refresh_integrations` runs desktop-cache tools that would act on the real session.
        .env("PATH", home.path().join("no-tools"))
        .output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success() && stdout.contains("1 passed"), "sandboxed run of {name} failed:\n{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
}

/// Names inside `directory`, sorted, or nothing when it does not exist.
pub fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(directory).map(|items| items.filter_map(Result::ok).map(|item| item.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    names
}

/// One archive member. Link targets are written verbatim, including `..` and absolute paths.
pub enum Member<'a> { Dir(&'a str), File(&'a str, &'a [u8], u32), Symlink(&'a str, &'a str), Hardlink(&'a str, &'a str) }

/// Bytes `security::is_elf` accepts; nothing in a test ever runs them.
pub const FAKE_ELF: &[u8] = b"\x7fELF\x02\x01\x01\0fake launcher";

/// Writes `members` to `directory/name` as an uncompressed tar archive and returns its path.
pub fn tar(directory: &Path, name: &str, members: &[Member]) -> PathBuf {
    let path = directory.join(name);
    let mut builder = Builder::new(fs::File::create(&path).unwrap());
    for member in members {
        let mut header = Header::new_gnu();
        match *member {
            Member::Dir(member) => { header.set_entry_type(EntryType::Directory); header.set_mode(0o755); header.set_size(0); builder.append_data(&mut header, member, &[][..]).unwrap(); }
            Member::File(member, data, mode) => { header.set_mode(mode); header.set_size(data.len() as u64); builder.append_data(&mut header, member, data).unwrap(); }
            Member::Symlink(member, target) => { header.set_entry_type(EntryType::Symlink); header.set_mode(0o777); header.set_size(0); builder.append_link(&mut header, member, target).unwrap(); }
            Member::Hardlink(member, target) => { header.set_entry_type(EntryType::Link); header.set_mode(0o644); header.set_size(0); builder.append_link(&mut header, member, target).unwrap(); }
        }
    }
    builder.into_inner().unwrap();
    path
}

/// Writes a ZIP archive holding regular files and symbolic links; hard links do not exist in ZIP.
pub fn zip(directory: &Path, name: &str, members: &[Member]) -> PathBuf {
    let path = directory.join(name);
    let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for member in members {
        match *member {
            Member::Dir(member) => writer.add_directory(member, options).unwrap(),
            Member::File(member, data, mode) => { writer.start_file(member, options.unix_permissions(mode)).unwrap(); std::io::Write::write_all(&mut writer, data).unwrap(); }
            Member::Symlink(member, target) => writer.add_symlink(member, target, options).unwrap(),
            Member::Hardlink(..) => panic!("ZIP archives cannot hold hard links"),
        }
    }
    writer.finish().unwrap();
    path
}
