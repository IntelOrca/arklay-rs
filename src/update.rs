//! Release and nightly update checks, downloads and the in-place swap.
//!
//! Both channels publish a `build.json` next to the binary asset. Nightly uses
//! GitHub's rolling `nightly` pre-release; release uses the latest normal
//! release. A build only updates itself when CI baked in `ARKLAY_CHANNEL`;
//! local `dev` builds report that updates are unavailable.
//!
//! The executable is replaced by renaming the running image to
//! `<exe>.old`, moving the verified download into place, and restarting. The
//! previous image stays for the Update screen's rollback row; the next build
//! removes a leftover `.rolledback` image on startup.

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::build_info;
use crate::config::UpdateChannel;

/// The release channel's feed (the latest normal release).
pub const RELEASE_FEED: &str =
    "https://github.com/arklay-engine/arklay/releases/latest/download/build.json";
/// The nightly channel's feed (the rolling pre-release).
pub const NIGHTLY_FEED: &str =
    "https://github.com/arklay-engine/arklay/releases/download/nightly/build.json";
/// A development override for the feed URL, used by local update tests.
pub const FEED_ENV: &str = "ARKLAY_UPDATE_FEED";

/// One update the launcher can offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub channel: String,
    pub version: String,
    pub sha: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Deserialize)]
struct Feed {
    channel: String,
    version: String,
    sha: String,
    #[serde(default)]
    assets: HashMap<String, Asset>,
}

#[derive(Deserialize)]
struct Asset {
    url: String,
    sha256: String,
}

/// The feed URL for `channel`, unless `ARKLAY_UPDATE_FEED` overrides it.
pub fn feed_url(channel: UpdateChannel) -> String {
    env::var(FEED_ENV)
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| {
            match channel {
                UpdateChannel::Release => RELEASE_FEED,
                UpdateChannel::Nightly => NIGHTLY_FEED,
            }
            .to_string()
        })
}

/// The asset key for this build's platform.
pub fn platform_key() -> &'static str {
    if cfg!(windows) {
        "windows-x86_64"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "macos-aarch64"
        } else {
            "macos-x86_64"
        }
    } else {
        "linux-x86_64"
    }
}

/// Check `channel` for a newer published build.
pub fn check(channel: UpdateChannel) -> Result<Option<UpdateInfo>> {
    if !build_info::is_published() {
        return Ok(None);
    }
    let url = feed_url(channel);
    let body = ureq::get(&url)
        .timeout(Duration::from_secs(10))
        .call()
        .with_context(|| format!("failed to fetch {url}"))?
        .into_string()
        .context("failed to read the update feed")?;
    let body = body.strip_prefix('\u{feff}').unwrap_or(&body);
    let feed: Feed = serde_json::from_str(body).context("failed to parse the update feed")?;
    let Some(asset) = feed.assets.get(platform_key()) else {
        bail!("the update feed has no {} build", platform_key());
    };
    if !newer_for(
        build_info::VERSION,
        build_info::COMMIT,
        &feed.channel,
        &feed.version,
        &feed.sha,
    ) {
        return Ok(None);
    }
    Ok(Some(UpdateInfo {
        channel: feed.channel,
        version: feed.version,
        sha: feed.sha,
        url: asset.url.clone(),
        sha256: asset.sha256.clone(),
    }))
}

/// Whether a feed build is newer than the running build. Kept pure so the
/// version rules can be tested without a published executable.
pub fn newer_for(
    current_version: &str,
    current_commit: &str,
    channel: &str,
    version: &str,
    sha: &str,
) -> bool {
    match channel {
        "nightly" => !sha.is_empty() && sha != current_commit,
        "release" => semver_gt(version, current_version),
        _ => false,
    }
}

/// Numeric `major.minor.patch` comparison; a release feed is not expected to
/// carry a pre-release suffix.
fn semver_gt(available: &str, current: &str) -> bool {
    fn parts(text: &str) -> [u64; 3] {
        let text = text.trim_start_matches('v');
        let text = text.split('-').next().unwrap_or(text);
        let mut out = [0; 3];
        for (slot, part) in out.iter_mut().zip(text.split('.')) {
            *slot = part.parse().unwrap_or(0);
        }
        out
    }
    parts(available) > parts(current)
}

/// Download `info` to `dest`, verifying its SHA-256. `progress` receives
/// `(bytes done, bytes total)`; a zero total means the server did not send a
/// length. Cancellation removes the partial file.
pub fn download(
    info: &UpdateInfo,
    dest: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    if info.sha256.trim().is_empty() {
        bail!("the update feed has no checksum");
    }
    let response = ureq::get(&info.url)
        .timeout(Duration::from_secs(60))
        .call()
        .with_context(|| format!("failed to download {}", info.url))?;
    let total = response
        .header("Content-Length")
        .and_then(|length| length.parse::<u64>().ok())
        .unwrap_or(0);
    let mut reader = response.into_reader();
    let mut file =
        File::create(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    let result = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err(anyhow::anyhow!("update cancelled"));
        }
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(err) => break Err(err.into()),
        };
        if read == 0 {
            break Ok(());
        }
        if let Err(err) = file.write_all(&buffer[..read]) {
            break Err(err.into());
        }
        hasher.update(&buffer[..read]);
        done += read as u64;
        progress(done, total);
    };
    drop(file);
    if let Err(err) = result {
        let _ = fs::remove_file(dest);
        return Err(err);
    }
    let digest = format!("{:x}", hasher.finalize());
    if !digest.eq_ignore_ascii_case(info.sha256.trim()) {
        let _ = fs::remove_file(dest);
        bail!("the downloaded update failed its checksum");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dest, fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// The sidecar path next to `exe` with `suffix` appended.
fn sidecar(exe: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", exe.display()))
}

/// The downloaded update's staging path.
pub fn staging_path(exe: &Path) -> PathBuf {
    sidecar(exe, ".new")
}

/// The previous image kept for rollback.
pub fn old_path(exe: &Path) -> PathBuf {
    sidecar(exe, ".old")
}

/// The image displaced by a rollback, deleted on the next start.
fn rollback_path(exe: &Path) -> PathBuf {
    sidecar(exe, ".rolledback")
}

/// The running executable.
pub fn current_exe() -> Result<PathBuf> {
    env::current_exe().context("failed to locate the running executable")
}

/// Install `new` over `exe`, keeping the previous image as `<exe>.old`.
pub fn install_at(exe: &Path, new: &Path) -> Result<()> {
    let old = old_path(exe);
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).with_context(|| format!("failed to move {} aside", exe.display()))?;
    if let Err(err) = fs::rename(new, exe) {
        let _ = fs::rename(&old, exe);
        return Err(err).with_context(|| format!("failed to install {}", exe.display()));
    }
    Ok(())
}

/// Install the staged update over the running executable.
pub fn install_staged() -> Result<()> {
    let exe = current_exe()?;
    let new = staging_path(&exe);
    install_at(&exe, &new)
}

/// Swap the previous image back over `exe`; the displaced image is removed on
/// the next start.
pub fn rollback_at(exe: &Path) -> Result<()> {
    let old = old_path(exe);
    if !old.is_file() {
        bail!("there is no previous build to roll back to");
    }
    let backup = rollback_path(exe);
    let _ = fs::remove_file(&backup);
    fs::rename(exe, &backup).with_context(|| format!("failed to move {} aside", exe.display()))?;
    if let Err(err) = fs::rename(&old, exe) {
        let _ = fs::rename(&backup, exe);
        return Err(err).with_context(|| format!("failed to restore {}", exe.display()));
    }
    Ok(())
}

/// Roll back the running executable.
pub fn rollback() -> Result<()> {
    rollback_at(&current_exe()?)
}

/// Whether a previous image is waiting.
pub fn rollback_available() -> bool {
    current_exe()
        .map(|exe| old_path(&exe).is_file())
        .unwrap_or(false)
}

/// Start a fresh launcher process from the running executable.
pub fn restart() -> Result<()> {
    let exe = current_exe()?;
    std::process::Command::new(exe)
        .spawn()
        .context("failed to restart arklay")?;
    Ok(())
}

/// Remove leftovers from an interrupted update or rollback. Called once at
/// startup, before the launcher draws.
pub fn cleanup() {
    let Ok(exe) = current_exe() else {
        return;
    };
    let _ = fs::remove_file(rollback_path(&exe));
    let _ = fs::remove_file(staging_path(&exe));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_versions_compare_numerically() {
        assert!(semver_gt("v0.2.0", "0.1.9"));
        assert!(semver_gt("0.1.10", "0.1.9"));
        assert!(!semver_gt("0.1.0", "0.1.0"));
        assert!(!semver_gt("v0.1.0", "0.2.0"));
        assert!(!semver_gt("0.1.0-nightly", "0.1.0"));
    }

    #[test]
    fn channel_rules_compare_the_right_identity() {
        assert!(newer_for("0.1.0", "aaa", "nightly", "", "bbb"));
        assert!(!newer_for("0.1.0", "aaa", "nightly", "", "aaa"));
        assert!(newer_for("0.1.0", "aaa", "release", "0.2.0", "bbb"));
        assert!(!newer_for("0.1.0", "aaa", "release", "0.1.0", "bbb"));
        assert!(!newer_for("0.1.0", "aaa", "beta", "9.9.9", "bbb"));
    }

    #[test]
    fn a_feed_selects_the_platform_asset() {
        let text = r#"{
            "channel": "nightly",
            "version": "0.1.0-nightly",
            "sha": "abc",
            "assets": {
                "windows-x86_64": { "url": "https://example.test/a.exe", "sha256": "11" },
                "linux-x86_64": { "url": "https://example.test/a", "sha256": "22" }
            }
        }"#;
        let feed: Feed = serde_json::from_str(text).unwrap();
        assert_eq!(feed.channel, "nightly");
        assert_eq!(feed.assets[platform_key()].sha256.len(), 2);
    }

    #[test]
    fn downloading_verifies_the_checksum() {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = b"fake engine bytes".to_vec();
        let expected = format!("{:x}", Sha256::digest(&body));
        let server = std::thread::spawn({
            let body = body.clone();
            move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let dir = std::env::temp_dir().join(format!("arklay-download-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("arklay.exe.new");
        let info = UpdateInfo {
            channel: "nightly".to_string(),
            version: "0.1.0-nightly".to_string(),
            sha: "abc".to_string(),
            url: format!("http://{address}/arklay.exe"),
            sha256: expected,
        };
        let cancel = AtomicBool::new(false);
        download(&info, &dest, &cancel, |_, _| {}).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        server.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn installing_and_rolling_back_moves_the_sidecars() {
        let dir = std::env::temp_dir().join(format!("arklay-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("arklay.exe");
        let new = dir.join("arklay.exe.new");
        fs::write(&exe, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        install_at(&exe, &new).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(fs::read(old_path(&exe)).unwrap(), b"old");
        rollback_at(&exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        assert!(!old_path(&exe).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
