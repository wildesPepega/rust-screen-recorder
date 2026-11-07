//! Self-updater: on every daemon start, check GitHub for a newer release and install it.
//!
//! * runs *before* capture starts, so no replay buffer is lost
//! * verifies the `.sha256` published with the release
//! * replaces the binary atomically (write temp file + rename) and re-executes it
//! * never touches development builds (`…/target/…`) or binaries in read-only locations
//!   (e.g. `/usr/bin` from a distribution package) – those only get a notice

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const REPO: &str = "wildesPepega/rust-screen-recorder";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Set for the re-executed daemon so it can tell the user what happened.
pub const ENV_UPDATED_FROM: &str = "RSR_UPDATED_FROM";

#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub version: (u64, u64, u64),
    pub tarball: String,
    pub checksum: Option<String>,
}

pub enum Outcome {
    UpToDate,
    /// A newer version exists but cannot be installed automatically (reason).
    Available(String, String),
    /// Installed; the caller should re-execute the binary.
    Installed(String),
}

pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

fn api_url() -> String {
    // Overridable for tests / mirrors.
    std::env::var("RSR_UPDATE_URL").unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"))
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("rust-screen-recorder/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(4))
        .timeout(timeout)
        .build()?)
}

pub fn latest() -> Result<Release> {
    let r = client(Duration::from_secs(8))?
        .get(api_url())
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("cannot reach GitHub")?;
    if !r.status().is_success() {
        bail!("GitHub answered {}", r.status());
    }
    let v: serde_json::Value = r.json()?;
    let tag = v["tag_name"].as_str().ok_or_else(|| anyhow!("release has no tag"))?.to_string();
    let version = parse_version(&tag).ok_or_else(|| anyhow!("unparsable release tag {tag}"))?;
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let find = |suffix: &str| {
        assets.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            name.ends_with(suffix).then(|| a["browser_download_url"].as_str().map(|s| s.to_string()))?
        })
    };
    let tarball = find("-x86_64-linux.tar.gz").ok_or_else(|| anyhow!("release {tag} has no x86_64 Linux build"))?;
    let checksum = find("-x86_64-linux.tar.gz.sha256");
    Ok(Release { tag, version, tarball, checksum })
}

/// Why the running binary must not be replaced (None = it may be updated).
pub fn not_updatable(exe: &Path) -> Option<String> {
    let s = exe.to_string_lossy();
    if s.contains("/target/release/") || s.contains("/target/debug/") {
        return Some("development build".into());
    }
    let dir = exe.parent()?;
    let probe = dir.join(format!(".rsr-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(probe);
            None
        }
        Err(_) => Some(format!("{} is not writable (installed by a package manager?)", dir.display())),
    }
}

/// Check for a newer release and install it over `exe`.
pub fn check_and_install(exe: &Path) -> Result<Outcome> {
    let rel = latest()?;
    let current = parse_version(CURRENT).unwrap_or((0, 0, 0));
    if rel.version <= current {
        return Ok(Outcome::UpToDate);
    }
    if let Some(why) = not_updatable(exe) {
        return Ok(Outcome::Available(rel.tag, why));
    }
    install(&rel, exe)?;
    Ok(Outcome::Installed(rel.tag))
}

fn install(rel: &Release, exe: &Path) -> Result<()> {
    let http = client(Duration::from_secs(180))?;
    let mut data = Vec::new();
    http.get(&rel.tarball).send()?.error_for_status()?.read_to_end(&mut data)?;
    let digest: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    match &rel.checksum {
        Some(url) => {
            let expected = http.get(url).send()?.error_for_status()?.text()?;
            let expected = expected.split_whitespace().next().unwrap_or("").to_lowercase();
            if expected != digest {
                bail!("checksum mismatch for {} – update aborted", rel.tag);
            }
        }
        None => bail!("release {} has no checksum file – update aborted", rel.tag),
    }

    // Unpack with the system tar into a private temp dir.
    let work = crate::session::log_dir().join(format!("update-{}", rel.tag));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let tgz = work.join("release.tar.gz");
    std::fs::write(&tgz, &data)?;
    let st = std::process::Command::new("tar").arg("-xzf").arg(&tgz).arg("-C").arg(&work).status()?;
    if !st.success() {
        bail!("could not unpack the update");
    }
    let new_bin = find_binary(&work).ok_or_else(|| anyhow!("update archive contains no rust-screen-recorder binary"))?;

    // Sanity check: the new binary must start.
    let ok = std::process::Command::new(&new_bin).arg("--help").output().map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        bail!("the downloaded binary does not run on this system (missing libraries?) – keeping the current version");
    }

    // Atomic replace: copy next to the target, then rename over it.
    let dir = exe.parent().ok_or_else(|| anyhow!("bad install path"))?;
    let tmp = dir.join(format!(".rust-screen-recorder.update-{}", std::process::id()));
    std::fs::copy(&new_bin, &tmp)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, exe).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    let _ = std::fs::remove_dir_all(&work);
    log::info!("updated {} to {}", exe.display(), rel.tag);
    Ok(())
}

fn find_binary(dir: &Path) -> Option<PathBuf> {
    for e in walk(dir) {
        if e.file_name().and_then(|n| n.to_str()) == Some("rust-screen-recorder") && e.is_file() {
            return Some(e);
        }
    }
    None
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10"), Some((1, 10, 0)));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
    }
    #[test]
    fn dev_builds_are_never_replaced() {
        assert!(not_updatable(Path::new("/home/x/proj/target/release/rust-screen-recorder")).is_some());
    }
}
