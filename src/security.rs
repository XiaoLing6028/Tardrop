//! Small checks shared by the installer before a launcher is made public.

use std::{fmt, fs, io::Read, path::{Path, PathBuf}};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// A refusal caused by what the archive contains, as opposed to an I/O or format failure. The UI
/// recognises it by type and is the only place that may offer to retry under `Policy::Override`.
/// A refusal from the pre-extraction survey also carries its `RiskAssessment`, so the prompt can
/// say how much is being trusted rather than quoting only the first member that tripped a check.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct Rejected { pub reason: String, pub risk: Option<RiskAssessment> }

impl Rejected {
    /// A content refusal with no survey behind it; the prompt shows only `reason`.
    pub fn new(reason: impl Into<String>) -> Self { Self { reason: reason.into(), risk: None } }
}

impl From<RiskAssessment> for Rejected {
    fn from(risk: RiskAssessment) -> Self { Self { reason: format!("Risk: {}", risk.summary()), risk: Some(risk) } }
}

/// Whether content checks are enforced. `Override` is only ever chosen explicitly by the user after
/// seeing a `Rejected` reason; it never allows a write outside the private staging directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy { #[default] Enforce, Override }

/// The user's answer to a `Rejected` prompt. It lives here, not in the UI, so the one rule it
/// encodes — only an explicit approval ever produces `Override` — is testable without a display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision { Approve, Deny }

impl Decision {
    /// Approval retries the same archive under `Override`. Denial retries nothing, which is exactly
    /// what a refusal did before risk review existed: the staging directory is already gone.
    pub fn retry_policy(self) -> Option<Policy> { match self { Decision::Approve => Some(Policy::Override), Decision::Deny => None } }
}

/// Which kind of link a member is. Both are refused under `Policy::Enforce`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind { Symbolic, Hard }

/// One link member found by `archive::survey`, and whether its target leaves the package folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkFinding { pub path: PathBuf, pub target: PathBuf, pub kind: LinkKind, pub escapes: bool }

/// Everything a read-only survey found that blocks an install by default. Nothing has been written
/// when this exists, so the user decides with the whole archive in view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Findings { pub links: Vec<LinkFinding>, pub install_scripts: Vec<PathBuf> }

impl Findings {
    /// True when the archive needs explicit approval before anything is extracted.
    pub fn blocks(&self) -> bool { !self.links.is_empty() || !self.install_scripts.is_empty() }
}

/// Coarse, countable tiers, so the prompt can always explain exactly why a level was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel { Low, Medium, High }

impl fmt::Display for RiskLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self { RiskLevel::Low => "Low", RiskLevel::Medium => "Medium", RiskLevel::High => "High" })
    }
}

/// The level shown before the approve/deny prompt, with the counted facts that produced it and a
/// per-member list. Member names are archive text; the UI renders them only as plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RiskAssessment { pub level: RiskLevel, pub reasons: Vec<String>, pub details: Vec<String> }

impl RiskAssessment {
    /// High: any `install.sh`, or four or more links. Medium: two or three links, or any link that
    /// leaves the package. Low: at most one link, staying inside. An `install.sh` outranks every
    /// link count because it signals a package built to be run by hand, which TarDrop never does.
    pub fn of(findings: &Findings) -> Self {
        let links = findings.links.len();
        let escaping = findings.links.iter().filter(|link| link.escapes).count();
        let level = if !findings.install_scripts.is_empty() || links >= 4 { RiskLevel::High }
            else if links >= 2 || escaping > 0 { RiskLevel::Medium }
            else { RiskLevel::Low };
        let mut reasons = Vec::new();
        if !findings.install_scripts.is_empty() { reasons.push(format!("install.sh script found ({})", findings.install_scripts.len())); }
        // Escapes come first: they are what a reviewer most needs to see.
        for escapes in [true, false] {
            for kind in [LinkKind::Symbolic, LinkKind::Hard] {
                let count = findings.links.iter().filter(|link| link.kind == kind && link.escapes == escapes).count();
                if count == 0 { continue; }
                let noun = match (kind, count) { (LinkKind::Symbolic, 1) => "symlink", (LinkKind::Symbolic, _) => "symlinks", (LinkKind::Hard, 1) => "hardlink", (LinkKind::Hard, _) => "hardlinks" };
                let place = match (escapes, count) { (true, 1) => "escapes the package", (true, _) => "escape the package", (false, _) => "inside the package" };
                reasons.push(format!("{count} {noun} {place}"));
            }
        }
        let mut details: Vec<String> = findings.install_scripts.iter().map(|path| format!("install.sh: {}", path.display())).collect();
        let mut ordered: Vec<_> = findings.links.iter().collect();
        ordered.sort_by_key(|link| !link.escapes);
        details.extend(ordered.into_iter().map(|link| {
            let kind = match link.kind { LinkKind::Symbolic => "symlink", LinkKind::Hard => "hardlink" };
            let place = if link.escapes { " (outside the package)" } else { "" };
            format!("{kind}: {} → {}{place}", link.path.display(), link.target.display())
        }));
        Self { level, reasons, details }
    }

    /// One line for the prompt and the log, e.g. `High — install.sh script found (1), 2 symlinks escape the package`.
    pub fn summary(&self) -> String {
        if self.reasons.is_empty() { format!("{} — no links or install scripts", self.level) } else { format!("{} — {}", self.level, self.reasons.join(", ")) }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn link(name: &str, kind: LinkKind, escapes: bool) -> LinkFinding {
        LinkFinding { path: PathBuf::from(name), target: PathBuf::from("target"), kind, escapes }
    }

    /// `count` in-package links, alternating kinds so both count toward the same total.
    fn internal_links(count: usize) -> Findings {
        let links = (0..count).map(|index| link(&format!("link{index}"), if index % 2 == 0 { LinkKind::Symbolic } else { LinkKind::Hard }, false)).collect();
        Findings { links, install_scripts: Vec::new() }
    }

    #[test]
    fn zero_links_is_not_blocked() {
        let findings = internal_links(0);
        assert!(!findings.blocks(), "a clean archive must install without a prompt");
        assert_eq!(RiskAssessment::of(&findings).level, RiskLevel::Low);
    }

    #[test]
    fn one_internal_link_is_low() {
        let findings = internal_links(1);
        assert!(findings.blocks());
        let risk = RiskAssessment::of(&findings);
        assert_eq!(risk.level, RiskLevel::Low);
        assert_eq!(risk.summary(), "Low — 1 symlink inside the package");
    }

    #[test]
    fn two_internal_links_are_medium() {
        let risk = RiskAssessment::of(&internal_links(2));
        assert_eq!(risk.level, RiskLevel::Medium);
        assert_eq!(risk.summary(), "Medium — 1 symlink inside the package, 1 hardlink inside the package");
    }

    #[test]
    fn three_internal_links_are_medium() {
        assert_eq!(RiskAssessment::of(&internal_links(3)).level, RiskLevel::Medium);
    }

    #[test]
    fn four_internal_links_are_high() {
        assert_eq!(RiskAssessment::of(&internal_links(4)).level, RiskLevel::High);
    }

    #[test]
    fn single_escaping_symlink_is_medium_not_low() {
        let findings = Findings { links: vec![link("lib", LinkKind::Symbolic, true)], install_scripts: Vec::new() };
        let risk = RiskAssessment::of(&findings);
        assert_eq!(risk.level, RiskLevel::Medium);
        assert_eq!(risk.summary(), "Medium — 1 symlink escapes the package");
    }

    #[test]
    fn single_escaping_hardlink_is_medium_not_low() {
        let findings = Findings { links: vec![link("data", LinkKind::Hard, true)], install_scripts: Vec::new() };
        assert_eq!(RiskAssessment::of(&findings).level, RiskLevel::Medium);
    }

    #[test]
    fn four_escaping_links_are_high() {
        let links = (0..4).map(|index| link(&format!("out{index}"), LinkKind::Symbolic, true)).collect();
        assert_eq!(RiskAssessment::of(&Findings { links, install_scripts: Vec::new() }).level, RiskLevel::High);
    }

    #[test]
    fn install_sh_alone_is_high() {
        let findings = Findings { links: Vec::new(), install_scripts: vec![PathBuf::from("App/install.sh")] };
        assert!(findings.blocks(), "install.sh must block even without any link");
        let risk = RiskAssessment::of(&findings);
        assert_eq!(risk.level, RiskLevel::High);
        assert_eq!(risk.summary(), "High — install.sh script found (1)");
        assert_eq!(risk.details, ["install.sh: App/install.sh"]);
    }

    #[test]
    fn install_sh_outranks_a_single_internal_link() {
        let findings = Findings { links: vec![link("lib", LinkKind::Symbolic, false)], install_scripts: vec![PathBuf::from("install.sh")] };
        assert_eq!(RiskAssessment::of(&findings).level, RiskLevel::High);
    }

    #[test]
    fn summary_lists_install_sh_then_escapes() {
        let findings = Findings {
            links: vec![link("a", LinkKind::Symbolic, true), link("b", LinkKind::Symbolic, true)],
            install_scripts: vec![PathBuf::from("install.sh")],
        };
        let risk = RiskAssessment::of(&findings);
        assert_eq!(risk.summary(), "High — install.sh script found (1), 2 symlinks escape the package");
        assert_eq!(Rejected::from(risk.clone()).to_string(), format!("Risk: {}", risk.summary()));
    }

    #[test]
    fn details_put_escaping_links_first() {
        let findings = Findings { links: vec![link("inside", LinkKind::Symbolic, false), link("outside", LinkKind::Hard, true)], install_scripts: Vec::new() };
        let risk = RiskAssessment::of(&findings);
        assert_eq!(risk.details, ["hardlink: outside → target (outside the package)", "symlink: inside → target"]);
    }

    #[test]
    fn only_approval_overrides() {
        assert_eq!(Decision::Approve.retry_policy(), Some(Policy::Override));
        assert_eq!(Decision::Deny.retry_policy(), None);
    }
}
