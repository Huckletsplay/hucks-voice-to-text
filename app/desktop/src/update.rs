//! Check for Updates.
//!
//! Asks GitHub for the latest release **only when he chooses it** from the menu — there is no
//! background check, and this is the program's only network connection. It mirrors Huck's Snip 'n'
//! Clip:
//!
//! - an update is offered only for a strictly newer `v<major>.<minor>.<patch>` release that carries
//!   exactly this platform's download and checksum, named by [`download_name`] and
//!   [`checksum_name`], served from this repository's GitHub release downloads over HTTPS;
//! - the download goes to a temporary folder, never anywhere of his, and is offered only after
//!   its size matches GitHub's and its SHA-256 matches the published checksum;
//! - macOS: the app never installs over itself - he opens the DMG and drags the new copy across.
//!   Windows: the verified installer is started and the app steps aside for it, as Snip 'n' Clip
//!   does.
//!
//! The rules are plain functions with tests; the network and hashing go through the system's own
//! tools (`curl` and `shasum` on macOS, `curl.exe` and `certutil` on Windows), so there is no HTTP
//! or crypto dependency to carry.

use std::path::PathBuf;
use std::process::Command;

pub const REPOSITORY: &str = "Huckletsplay/hucks-voice-to-text";
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/Huckletsplay/hucks-voice-to-text/releases/latest";
const DOWNLOAD_PREFIX: &str =
    "https://github.com/Huckletsplay/hucks-voice-to-text/releases/download/";

/// The one place the release asset names live. `app/scripts/release.sh` produces exactly these;
/// a future signed or Intel channel has to be added here on purpose, or the updater refuses it.
#[cfg(not(windows))]
pub fn download_name(version: &str) -> String {
    format!("HucksVoiceToText-{version}-macOS-arm64-unsigned-beta.dmg")
}

/// Snip 'n' Clip's Windows naming: one installer per release, `-windows-x64-setup.exe`.
#[cfg(windows)]
pub fn download_name(version: &str) -> String {
    format!("HucksVoiceToText-{version}-windows-x64-setup.exe")
}

const PLATFORM: &str = if cfg!(windows) { "Windows" } else { "Mac" };

pub fn checksum_name(version: &str) -> String {
    format!("{}.sha256.txt", download_name(version))
}

/// `major.minor.patch`, with an optional leading `v`. Anything else is not a release version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

pub fn parse_version(text: &str) -> Option<Version> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let v = Version(next()?, next()?, next()?);
    parts.next().is_none().then_some(v)
}

/// A newer release, ready to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub version: String,
    pub download_url: String,
    pub download_size: u64,
    pub checksum_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    UpToDate { latest: String },
    Available(Offer),
}

/// Read GitHub's "latest release" answer against the running version.
pub fn read_release(json: &str, current: &str) -> Result<Check, String> {
    let release: serde_json::Value =
        serde_json::from_str(json).map_err(|_| "GitHub's answer could not be read.".to_string())?;
    let tag = release["tag_name"].as_str().unwrap_or_default();
    let latest = parse_version(tag)
        .ok_or_else(|| format!("The latest release has an unexpected version tag ({tag:?})."))?;
    let running = parse_version(current)
        .ok_or_else(|| format!("This copy has an unexpected version ({current:?})."))?;
    let version = tag.trim_start_matches('v').to_string();
    if latest <= running {
        return Ok(Check::UpToDate { latest: version });
    }

    let asset = |name: &str| {
        release["assets"].as_array()?.iter().find(|a| a["name"].as_str() == Some(name)).and_then(
            |a| {
                let url = a["browser_download_url"].as_str()?;
                url.starts_with(DOWNLOAD_PREFIX).then(|| (url.to_string(), a["size"].as_u64()))
            },
        )
    };
    let missing = || format!("Version {version} is out, but its {PLATFORM} download is missing.");
    let (download_url, download_size) = asset(&download_name(&version)).ok_or_else(missing)?;
    let (checksum_url, _) = asset(&checksum_name(&version)).ok_or_else(missing)?;
    let download_size = download_size.ok_or_else(missing)?;
    Ok(Check::Available(Offer { version: version.clone(), download_url, download_size, checksum_url }))
}

/// The published checksum file: exactly one line, `<64 hex>  <this DMG's name>`.
pub fn parse_checksum(text: &str, dmg: &str) -> Result<String, String> {
    let bad = || "The published checksum is not in the expected form.".to_string();
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let line = lines.next().ok_or_else(bad)?;
    if lines.next().is_some() {
        return Err(bad());
    }
    let mut fields = line.split_whitespace();
    let hash = fields.next().ok_or_else(bad)?.to_ascii_lowercase();
    let name = fields.next().ok_or_else(bad)?.trim_start_matches('*');
    if fields.next().is_some()
        || hash.len() != 64
        || !hash.chars().all(|c| c.is_ascii_hexdigit())
        || name != dmg
    {
        return Err(bad());
    }
    Ok(hash)
}

// ---------------------------------------------------------------------------- the network

/// The system's own curl - part of macOS, and of Windows 10 since 1803.
fn curl_program() -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join("System32").join("curl.exe")
    } else {
        PathBuf::from("/usr/bin/curl")
    }
}

fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new(curl_program());
    #[cfg(windows)]
    {
        // No console window flashing up behind the floating box.
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = command
        .args(["--fail", "--silent", "--show-error", "--location"])
        // HTTPS only, including every redirect GitHub makes to its download servers.
        .args(["--proto", "=https", "--proto-redir", "=https", "--tlsv1.2"])
        .args(args)
        .output()
        .map_err(|e| format!("Could not reach GitHub ({e})."))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if why.is_empty() { "Could not reach GitHub.".into() } else { why });
    }
    Ok(out.stdout)
}

/// Ask GitHub about the latest release.
pub fn fetch_latest(current: &str) -> Result<Check, String> {
    let agent = format!("HucksVoiceToText/{current}");
    let body = curl(&[
        "--max-time",
        "20",
        "--header",
        "Accept: application/vnd.github+json",
        "--user-agent",
        &agent,
        LATEST_RELEASE_API,
    ])?;
    read_release(&String::from_utf8_lossy(&body), current)
}

/// Where downloads wait. Per-user temporary space, cleared before and on any failure.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("HucksVoiceToText-Update")
}

/// Download the DMG and prove it is the published one. Returns its path only if it is.
pub fn download(offer: &Offer) -> Result<PathBuf, String> {
    let dir = download_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not prepare the download ({e})."))?;
    let result = download_into(offer, &dir);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

fn download_into(offer: &Offer, dir: &std::path::Path) -> Result<PathBuf, String> {
    let name = download_name(&offer.version);
    let expected = parse_checksum(
        &String::from_utf8_lossy(&curl(&["--max-time", "30", &offer.checksum_url])?),
        &name,
    )?;

    let dmg = dir.join(&name);
    let dmg_text = dmg.to_string_lossy().to_string();
    curl(&["--max-time", "900", "--output", &dmg_text, &offer.download_url])?;

    let size = std::fs::metadata(&dmg).map(|m| m.len()).unwrap_or(0);
    if size != offer.download_size {
        return Err("The download is incomplete — its size does not match GitHub's.".into());
    }
    let actual = sha256_of(&dmg_text)?;
    if actual != expected {
        return Err("The download does not match its published SHA-256 checksum, so it was \
                    deleted."
            .into());
    }
    Ok(dmg)
}

/// SHA-256 of a file, lower-case hex, from the system's own tool.
fn sha256_of(path: &str) -> Result<String, String> {
    let fail = |e: std::io::Error| format!("Could not check the download ({e}).");
    #[cfg(windows)]
    let out = {
        use std::os::windows::process::CommandExt;
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Command::new(PathBuf::from(root).join("System32").join("certutil.exe"))
            .args(["-hashfile", path, "SHA256"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .map_err(fail)?
    };
    #[cfg(not(windows))]
    let out = Command::new("/usr/bin/shasum").args(["-a", "256", path]).output().map_err(fail)?;
    Ok(read_sha256(&String::from_utf8_lossy(&out.stdout)).unwrap_or_default())
}

/// The first 64-hex-digit hash in a tool's output. `certutil` puts a heading line first, and older
/// versions space the bytes apart; `shasum` puts the hash first.
fn read_sha256(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let first = line.split_whitespace().next()?;
        let joined: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        [first.to_string(), joined]
            .into_iter()
            .find(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
            .map(|h| h.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[(&str, &str, u64)]) -> String {
        let assets: Vec<_> = assets
            .iter()
            .map(|(n, u, s)| serde_json::json!({"name": n, "browser_download_url": u, "size": s}))
            .collect();
        serde_json::json!({"tag_name": tag, "assets": assets}).to_string()
    }

    fn url(version: &str, file: &str) -> String {
        format!("{DOWNLOAD_PREFIX}v{version}/{file}")
    }

    #[test]
    fn hashes_are_read_from_either_tool() {
        let hash = "ab".repeat(32);
        assert_eq!(read_sha256(&format!("{hash}  file.dmg\n")), Some(hash.clone()));
        let certutil = format!(
            "SHA256 hash of C:\\t\\file.exe:\r\n{}\r\nCertUtil: -hashfile command completed successfully.\r\n",
            hash.to_uppercase()
        );
        assert_eq!(read_sha256(&certutil), Some(hash.clone()));
        let spaced = hash.as_bytes().chunks(2).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(" ");
        assert_eq!(read_sha256(&format!("SHA256 hash of file:\n{spaced}\n")), Some(hash));
        assert_eq!(read_sha256("CertUtil: error\n"), None);
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(parse_version("v0.1.0"), Some(Version(0, 1, 0)));
        assert_eq!(parse_version("0.10.2"), Some(Version(0, 10, 2)));
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
        for bad in ["", "v1", "1.2", "1.2.3.4", "v1.2.x", "latest"] {
            assert_eq!(parse_version(bad), None, "{bad:?} is not a release version");
        }
    }

    #[test]
    fn the_same_or_an_older_release_is_up_to_date() {
        let json = release("v0.1.0", &[]);
        assert_eq!(read_release(&json, "0.1.0").unwrap(),
                   Check::UpToDate { latest: "0.1.0".into() });
        assert!(matches!(read_release(&json, "0.2.0").unwrap(), Check::UpToDate { .. }));
    }

    #[test]
    fn a_newer_release_with_both_files_is_offered() {
        let (dmg, sum) = (download_name("0.2.0"), checksum_name("0.2.0"));
        let json = release("v0.2.0", &[(&dmg, &url("0.2.0", &dmg), 123), (&sum, &url("0.2.0", &sum), 90)]);
        let Check::Available(offer) = read_release(&json, "0.1.0").unwrap() else {
            panic!("a newer release should be offered")
        };
        assert_eq!(offer.version, "0.2.0");
        assert_eq!(offer.download_size, 123);
        assert!(offer.download_url.ends_with(&dmg));
    }

    #[test]
    fn a_newer_release_missing_its_checksum_is_refused() {
        let dmg = download_name("0.2.0");
        let json = release("v0.2.0", &[(&dmg, &url("0.2.0", &dmg), 123)]);
        assert!(read_release(&json, "0.1.0").unwrap_err().contains("missing"));
    }

    #[test]
    fn a_download_from_anywhere_but_this_repository_is_refused() {
        let (dmg, sum) = (download_name("0.2.0"), checksum_name("0.2.0"));
        let elsewhere = format!("https://example.com/{dmg}");
        let json = release("v0.2.0", &[(&dmg, &elsewhere, 123), (&sum, &url("0.2.0", &sum), 90)]);
        assert!(read_release(&json, "0.1.0").is_err());
        let http = url("0.2.0", &dmg).replace("https://", "http://");
        let json = release("v0.2.0", &[(&dmg, &http, 123), (&sum, &url("0.2.0", &sum), 90)]);
        assert!(read_release(&json, "0.1.0").is_err());
    }

    #[test]
    fn a_differently_named_asset_is_not_taken_for_the_dmg() {
        let sum = checksum_name("0.2.0");
        let other = "HucksVoiceToText-0.2.0-macOS-universal.dmg";
        let json = release("v0.2.0", &[(other, &url("0.2.0", other), 123), (&sum, &url("0.2.0", &sum), 90)]);
        assert!(read_release(&json, "0.1.0").is_err());
    }

    #[test]
    fn checksum_files_must_name_this_dmg_on_one_line() {
        let dmg = download_name("0.2.0");
        let hash = "a".repeat(64);
        assert_eq!(parse_checksum(&format!("{hash}  {dmg}\n"), &dmg).unwrap(), hash);
        assert_eq!(parse_checksum(&format!("{}  *{dmg}", hash.to_uppercase()), &dmg).unwrap(), hash);
        assert!(parse_checksum(&format!("{hash}  other.dmg"), &dmg).is_err());
        assert!(parse_checksum(&format!("{hash}  {dmg}\n{hash}  {dmg}"), &dmg).is_err());
        assert!(parse_checksum(&format!("{}  {dmg}", "a".repeat(63)), &dmg).is_err());
        assert!(parse_checksum(&format!("{}  {dmg}", "g".repeat(64)), &dmg).is_err());
        assert!(parse_checksum("", &dmg).is_err());
    }
}
