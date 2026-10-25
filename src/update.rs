use std::fs;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::data_dir;

pub const REPO: &str = "Milkshak3s/iphone-photo-sync";
const ASSET_SUFFIX: &str = "-windows-x64.exe";
const SUMS_ASSET: &str = "SHA256SUMS.txt";

#[derive(Debug, Clone)]
pub struct Update {
    pub version: Version,
    asset_name: String,
    exe_url: String,
    sums_url: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
}

fn client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("iphone-photo-sync/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()?)
}

/// Asks GitHub for the latest published release and returns it if it is newer
/// than this build and carries a Windows exe plus a checksum file.
pub fn check() -> Result<Option<Update>> {
    let release: GhRelease = client(Duration::from_secs(30))?
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()?
        .json()?;
    pick(release, &current_version())
}

fn pick(release: GhRelease, current: &Version) -> Result<Option<Update>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = Version::parse(release.tag_name.trim_start_matches('v'))
        .with_context(|| format!("release tag `{}` is not a version", release.tag_name))?;
    if version <= *current {
        return Ok(None);
    }
    let exe = release
        .assets
        .iter()
        .find(|a| a.name.ends_with(ASSET_SUFFIX))
        .with_context(|| format!("release {version} has no *{ASSET_SUFFIX} asset"))?;
    let sums = release
        .assets
        .iter()
        .find(|a| a.name == SUMS_ASSET)
        .with_context(|| format!("release {version} has no {SUMS_ASSET}"))?;
    Ok(Some(Update {
        version,
        asset_name: exe.name.clone(),
        exe_url: exe.browser_download_url.clone(),
        sums_url: sums.browser_download_url.clone(),
    }))
}

fn expected_sha256(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == asset).then(|| hash.to_ascii_lowercase())
    })
}

/// Downloads the update, checks it against the release's SHA256SUMS.txt and swaps
/// it in for the running exe. The new version runs after [`relaunch`].
pub fn install(update: &Update) -> Result<()> {
    let client = client(Duration::from_secs(600))?;
    let sums = client
        .get(&update.sums_url)
        .send()?
        .error_for_status()?
        .text()?;
    let expected = expected_sha256(&sums, &update.asset_name)
        .with_context(|| format!("{SUMS_ASSET} does not list {}", update.asset_name))?;
    let bytes = client
        .get(&update.exe_url)
        .send()?
        .error_for_status()?
        .bytes()?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            update.asset_name
        );
    }

    fs::create_dir_all(data_dir())?;
    let staged = data_dir().join(format!("update-{}.exe", update.version));
    fs::write(&staged, &bytes)?;
    let result = self_replace::self_replace(&staged).context("replacing the running exe");
    let _ = fs::remove_file(&staged);
    result
}

/// Starts the (updated) exe hidden in the tray. The caller must exit right after;
/// the new process waits for this one to release the single-instance lock.
pub fn relaunch() -> Result<()> {
    Command::new(std::env::current_exe()?)
        .args(["--minimized", "--after-update"])
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|n| GhAsset {
                    name: n.to_string(),
                    browser_download_url: format!("https://example.invalid/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn picks_newer_release_with_assets() {
        let current = Version::new(0, 1, 0);
        let assets = ["iphone-photo-sync-0.2.0-windows-x64.exe", SUMS_ASSET];
        let update = pick(release("v0.2.0", &assets), &current).unwrap().unwrap();
        assert_eq!(update.version, Version::new(0, 2, 0));
        assert_eq!(update.asset_name, assets[0]);

        assert!(
            pick(release("v0.1.0", &assets), &current)
                .unwrap()
                .is_none()
        );
        assert!(
            pick(release("v0.0.9", &assets), &current)
                .unwrap()
                .is_none()
        );
        assert!(pick(release("v0.2.0", &assets[..1]), &current).is_err());
    }

    #[test]
    fn reads_sha256sums() {
        let sums = "ABC123  iphone-photo-sync-0.2.0-windows-x64.exe\ndef  other.txt\n";
        assert_eq!(
            expected_sha256(sums, "iphone-photo-sync-0.2.0-windows-x64.exe").as_deref(),
            Some("abc123")
        );
        assert_eq!(expected_sha256("ff *a.exe", "a.exe").as_deref(), Some("ff"));
        assert_eq!(expected_sha256(sums, "missing.exe"), None);
    }
}
