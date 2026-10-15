//! Builders for the archives unit tests feed to the installer. They are written with the same `tar`
//! and `zip` crates the extractor reads, so a test exercises the production parsing path.

use std::{fs, path::{Path, PathBuf}};
use tar::{Builder, EntryType, Header};

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
