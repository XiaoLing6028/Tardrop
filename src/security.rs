//! Small checks shared by the installer before a launcher is made public.

use std::{fs, io::Read, path::Path};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// A refusal caused by what the archive contains, as opposed to an I/O or format failure. The UI
/// recognises it by type and is the only place that may offer to retry under `Policy::Override`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Rejected(pub String);

/// Whether content checks are enforced. `Override` is only ever chosen explicitly by the user after
/// seeing a `Rejected` reason; it never allows a write outside the private staging directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy { #[default] Enforce, Override }

/// Hashes the input while reading it, both providing an audit value and detecting read failures.
pub fn archive_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).context("could not open archive")?;
    let mut hash = Sha256::new(); let mut buffer = [0_u8; 64 * 1024];
    loop { let count = file.read(&mut buffer)?; if count == 0 { break; } hash.update(&buffer[..count]); }
    Ok(format!("{:x}", hash.finalize()))
}

/// Checks ELF magic rather than trusting a filename. Shell scripts are deliberately not launchers.
pub fn is_elf(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else { return false; };
    let mut magic = [0; 4]; file.read_exact(&mut magic).is_ok() && magic == [0x7f, b'E', b'L', b'F']
}

/// Allows names that cannot inject a desktop-entry field or path component.
pub fn safe_desktop_value(value: &str) -> Result<()> {
    if value.is_empty() || value.contains(['\n', '\r', '\0']) { bail!("unsafe desktop-entry value") }
    Ok(())
}
