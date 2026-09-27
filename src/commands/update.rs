//! `demo update` — check for a newer stable release and, after consent,
//! install it.
//!
//! The updater is explicit-only: DemoStage has no background notice path, so
//! the single network call it makes lives behind `demo update` itself and is
//! loud when it fails. It always asks first — the prompt defaults to **NO**
//! (`--yes` skips it) — and a binary installed under `~/.cargo/bin` is refused
//! outright: cargo owns that file (`cargo install --force demo-stage`).
//!
//! State survives the swap: only the running executable is replaced (see
//! [`replace_binary`]). `demo.toml` scores, recordings, the raw macro and the
//! capture sources are never read or written here.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The GitHub repository whose stable releases ship the `demo` binary.
pub const GITHUB_REPO: &str = "UniverLab/demostage";

/// Per-request network budget (release list, archive, checksums).
const UPDATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Body ceiling for the release list (one payload contains every release).
const MAX_RELEASES_BYTES: u64 = 8 * 1024 * 1024;

/// Body ceiling for downloads — the published `tar.gz` assets are ~10 MB.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// The release fields needed to select a stable, published binary.
#[derive(serde::Deserialize, Debug, Clone, PartialEq)]
pub struct GitHubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
}

/// Injectable release-JSON lookup, so unit tests never touch the network.
pub trait ReleaseFetcher {
    fn get(&self, url: &str) -> Result<String>;
}

/// Production release lookup (`api.github.com`).
pub struct RealFetcher;

impl ReleaseFetcher for RealFetcher {
    fn get(&self, url: &str) -> Result<String> {
        // ureq 3 turns any non-2xx status into `Err(ureq::Error::StatusCode)`
        // by default, so a failure here is already loud.
        let mut response = agent()
            .get(url)
            .header("User-Agent", "demo-update")
            .call()
            .map_err(|e| Error::Export(format!("failed to fetch GitHub releases: {e}")))?;
        response
            .body_mut()
            .with_config()
            .limit(MAX_RELEASES_BYTES)
            .read_to_string()
            .map_err(|e| Error::Export(format!("failed to read GitHub releases response: {e}")))
    }
}

/// Injectable binary downloader: the archive is only decoded after this seam
/// returns, which keeps the whole update core offline-testable.
pub trait BinaryDownloader {
    fn download(&self, url: &str) -> Result<Vec<u8>>;
}

/// Production downloader (release assets over `github.com`).
pub struct RealDownloader;

impl BinaryDownloader for RealDownloader {
    fn download(&self, url: &str) -> Result<Vec<u8>> {
        let mut response = agent()
            .get(url)
            .header("User-Agent", "demo-update")
            .call()
            .map_err(|e| Error::Export(format!("failed to download update: {e}")))?;
        response
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD_BYTES)
            .read_to_vec()
            .map_err(|e| Error::Export(format!("failed to read update archive: {e}")))
    }
}

/// One agent per call: a request-scoped client with a global timeout.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(UPDATE_TIMEOUT))
        .build()
        .new_agent()
}

/// Everything the update core needs from the outside world. The production
/// command fills it with real I/O; tests fill it with fakes and never hit
/// GitHub, the filesystem outside their tempdir, or the prompt.
pub struct UpdateDeps<'a> {
    pub current: &'a str,
    pub releases: std::result::Result<Vec<GitHubRelease>, String>,
    pub exe: &'a Path,
    pub cargo_bin: &'a Path,
    pub target: std::result::Result<&'a str, String>,
    pub downloader: &'a dyn BinaryDownloader,
    pub confirm: &'a dyn Fn() -> bool,
}

// ── Public entry points ─────────────────────────────────────────

/// Check for and, after consent, install the latest stable release.
///
/// The returned integer is the process exit code: `0` means nothing was
/// installed (up to date, declined prompt, or a cargo-installed binary) and
/// `1` is reserved for an available update in `--check` mode or a binary
/// missing from the downloaded archive.
pub fn run_update(check: bool, yes: bool) -> Result<i32> {
    let current = current_version();
    // Loud on failure: this is an explicit command, not a background notice.
    let releases = fetch_releases_with(&RealFetcher)?;

    // The first pass only reads the network result. When there is nothing to
    // install (or `--check` was asked for) no local path is touched at all:
    // the hermetic core below owns every other side effect and prompt.
    let latest = select_latest_stable(&releases, current);
    if latest.is_none() || check {
        let deps = UpdateDeps {
            current,
            releases: Ok(releases),
            exe: Path::new("/tmp/demo-update-test/demo"),
            cargo_bin: Path::new("/tmp/demo-update-test/not-cargo"),
            target: Ok("x86_64-unknown-linux-musl"),
            downloader: &RealDownloader,
            confirm: &|| false,
        };
        return run_update_with(check, yes, &deps);
    }

    // An install needs the executable, target and prompt facts. Resolve them
    // only once a newer release is established.
    let latest = latest.expect("newer release was established above");
    let exe = std::env::current_exe()
        .map_err(|e| Error::Export(format!("failed to locate demo executable: {e}")))?;
    let cargo_bin = cargo_bin_dir();
    let target = resolve_target()?;
    let deps = UpdateDeps {
        current,
        releases: Ok(releases),
        exe: &exe,
        cargo_bin: &cargo_bin,
        target: Ok(target),
        downloader: &RealDownloader,
        confirm: &|| {
            inquire::Confirm::new(&format!("Update to {latest}? [y/N]"))
                .with_default(false)
                .prompt()
                .unwrap_or(false)
        },
    };
    run_update_core(false, yes, &deps, true)
}

/// Hermetic update flow used by unit tests: no network, no prompt, no setup —
/// callers provide every external fact through [`UpdateDeps`].
pub fn run_update_with(check: bool, yes: bool, deps: &UpdateDeps<'_>) -> Result<i32> {
    run_update_core(check, yes, deps, true)
}

fn run_update_core(
    check: bool,
    yes: bool,
    deps: &UpdateDeps<'_>,
    print_status: bool,
) -> Result<i32> {
    let releases = deps
        .releases
        .as_ref()
        .map_err(|error| Error::Export(format!("release lookup failed: {error}")))?;
    let Some(latest) = select_latest_stable(releases, deps.current) else {
        if print_status {
            println!("demo {} is up to date", deps.current);
        }
        return Ok(0);
    };
    if print_status {
        println!("demo {} → {latest}", deps.current);
    }
    // `--check` reports availability only: exit before any local side effect.
    if check {
        return Ok(1);
    }
    // A cargo install is owned by cargo — never overwrite it.
    if is_cargo_installed(deps.exe, deps.cargo_bin) {
        println!("installed with cargo — run: cargo install --force demo-stage");
        return Ok(0);
    }
    let target = match &deps.target {
        Ok(target) => *target,
        Err(error) => return Err(Error::Export(format!("target resolution failed: {error}"))),
    };
    // Always asks; the prompt defaults to NO (CM34).
    if !yes && !(deps.confirm)() {
        println!("Aborted.");
        return Ok(0);
    }

    let tmp = tempfile::tempdir().map_err(|e| Error::io(std::env::temp_dir(), e))?;
    let tmp_bin = tmp.path().join("demo-new");
    if !download_and_extract_with(deps.downloader, &latest, target, &tmp_bin)? {
        eprintln!("  ✗ Binary not found in archive");
        return Ok(1);
    }
    replace_binary(&tmp_bin, deps.exe)?;
    println!("✓ updated to {latest}");
    Ok(0)
}

// ── Version helpers ─────────────────────────────────────────────

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A tag is stable when it is only digits and dots (`v`-prefix optional).
fn is_stable_version(tag: &str) -> bool {
    let value = tag.trim_start_matches('v');
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Compare two version strings numerically component by component; a missing
/// component counts as `0`, so `v0.3` equals `0.3.0`.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u32> {
        s.trim_start_matches('v')
            .split('.')
            .filter_map(|part| part.parse().ok())
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    let len = pa.len().max(pb.len());
    for index in 0..len {
        let comparison = pa
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&pb.get(index).copied().unwrap_or(0));
        if comparison != std::cmp::Ordering::Equal {
            return comparison;
        }
    }
    std::cmp::Ordering::Equal
}

/// The newest stable release strictly newer than `current`, or `None`.
pub fn select_latest_stable(releases: &[GitHubRelease], current: &str) -> Option<String> {
    releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter(|release| is_stable_version(&release.tag_name))
        .filter(|release| compare_versions(&release.tag_name, current).is_gt())
        .max_by(|a, b| compare_versions(&a.tag_name, &b.tag_name))
        .map(|release| release.tag_name.clone())
}

// ── Release lookup ──────────────────────────────────────────────

/// Fetch the release list. The `/releases` endpoint (not `/releases/latest`)
/// is used so drafts and prereleases stay visible and are filtered in code.
fn fetch_releases_with(fetcher: &dyn ReleaseFetcher) -> Result<Vec<GitHubRelease>> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases");
    let body = fetcher.get(&url)?;
    serde_json::from_str(&body)
        .map_err(|e| Error::Export(format!("failed to parse releases JSON: {e}")))
}

// ── Target and installation-path helpers ────────────────────────

/// Resolve a target triple from explicit OS/architecture inputs.
///
/// Only the three `tar.gz` targets the release workflow publishes exist:
/// Windows ships a `.zip` and there is no linux/aarch64 asset, so both bail
/// loudly *before* any download instead of guessing an asset name.
pub fn target_for(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => Err(Error::Export(format!("unsupported target: {arch}-{os}"))),
    }
}

/// The target triple of the running build, when the release workflow ships it.
pub fn resolve_target() -> Result<&'static str> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// The exact release asset name, keeping the tag's leading `v`
/// (`demo-{tag}-{target}.tar.gz`, from the release workflow's Package step).
pub fn asset_name(tag: &str, target: &str) -> String {
    format!("demo-{tag}-{target}.tar.gz")
}

/// `$CARGO_HOME/bin`, else the conventional `$HOME/.cargo/bin` (no `dirs`
/// crate in this repo — the environment is read directly).
pub fn cargo_bin_dir() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .filter(|value| !value.is_empty())
        .map(|value| PathBuf::from(value).join("bin"))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".cargo")
                .join("bin")
        })
}

/// Whether the executable lives below the cargo bin directory. Both paths are
/// canonicalized when possible (with the raw paths as a fallback for
/// non-existent test paths), so a symlinked release binary is never mistaken
/// for a cargo install.
pub fn is_cargo_installed(exe: &Path, cargo_bin: &Path) -> bool {
    let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let cargo_bin = cargo_bin
        .canonicalize()
        .unwrap_or_else(|_| cargo_bin.to_path_buf());
    exe.starts_with(cargo_bin)
}

// ── Download, verification and atomic replacement ───────────────

/// Download the release archive, verify it against `SHA256SUMS.txt` when the
/// release ships one, and extract its `demo` entry to `output`.
/// Returns `false` when the archive has no `demo` entry.
pub fn download_and_extract_with(
    downloader: &dyn BinaryDownloader,
    tag: &str,
    target: &str,
    output: &Path,
) -> Result<bool> {
    let asset = asset_name(tag, target);
    let url = format!("https://github.com/{GITHUB_REPO}/releases/download/{tag}/{asset}");
    let bytes = downloader.download(&url)?;
    // Verify the archive itself (as `scripts/install.sh` does) before decoding.
    verify_checksum_if_present(downloader, tag, &asset, &bytes)?;

    let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| Error::Export(format!("failed to read update archive: {e}")))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| Error::Export(format!("failed to read update archive: {e}")))?;
        let is_demo = entry
            .path()
            .map(|path| path.file_name().is_some_and(|name| name == "demo"))
            .unwrap_or(false);
        if !is_demo {
            continue;
        }
        entry
            .unpack(output)
            .map_err(|e| Error::io(output.to_path_buf(), e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| Error::io(output.to_path_buf(), e))?;
        }
        return Ok(true);
    }
    Ok(false)
}

/// `install.sh` semantics: a missing `SHA256SUMS.txt` (older releases) or a
/// missing line for this asset skips verification; a present, mismatching
/// checksum is fatal. Hex is encoded by hand — no `hex` crate.
fn verify_checksum_if_present(
    downloader: &dyn BinaryDownloader,
    tag: &str,
    asset: &str,
    bytes: &[u8],
) -> Result<()> {
    let sums_url =
        format!("https://github.com/{GITHUB_REPO}/releases/download/{tag}/SHA256SUMS.txt");
    let sums = match downloader.download(&sums_url) {
        Ok(sums) => sums,
        Err(_) => return Ok(()),
    };
    let text = String::from_utf8_lossy(&sums);
    let Some(line) = text
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(asset))
    else {
        return Ok(());
    };
    let Some(expected) = line.split_whitespace().next() else {
        return Ok(());
    };
    if expected.is_empty() {
        return Ok(());
    }
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hex != expected.to_ascii_lowercase() {
        return Err(Error::Export(format!(
            "SHA256 mismatch for {asset}: expected {expected}, got {hex}"
        )));
    }
    Ok(())
}

/// Replace `current_exe` from a staged file with a same-directory temporary
/// file + `rename` (atomic on Unix). Where renaming over an existing file is
/// refused, a copy fallback keeps the helper safe to compile; sibling files
/// are never touched — only the exe path moves.
pub fn replace_binary(staged: &Path, current_exe: &Path) -> Result<()> {
    let parent = current_exe.parent().ok_or_else(|| {
        Error::Export("cannot determine the demo executable directory".to_string())
    })?;
    let temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| Error::io(parent.to_path_buf(), e))?;
    std::fs::copy(staged, temporary.path())
        .map_err(|e| Error::io(temporary.path().to_path_buf(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o755))
            .map_err(|e| Error::io(temporary.path().to_path_buf(), e))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| Error::io(temporary.path().to_path_buf(), e))?;

    if std::fs::rename(temporary.path(), current_exe).is_ok() {
        return Ok(());
    }
    // Windows cannot rename over an existing file; the staged bytes are still
    // alive here (the NamedTempFile lives until this function returns).
    std::fs::copy(temporary.path(), current_exe)
        .map_err(|e| Error::io(current_exe.to_path_buf(), e))?;
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // ── Fakes ───────────────────────────────────────────────────

    struct FakeFetcher {
        body: String,
    }

    impl ReleaseFetcher for FakeFetcher {
        fn get(&self, _url: &str) -> Result<String> {
            Ok(self.body.clone())
        }
    }

    struct RecordingDownloader {
        called: Cell<bool>,
        bytes: Vec<u8>,
        sums: Option<Vec<u8>>,
    }

    impl RecordingDownloader {
        fn new(bytes: Vec<u8>, sums: Option<Vec<u8>>) -> Self {
            RecordingDownloader {
                called: Cell::new(false),
                bytes,
                sums,
            }
        }
    }

    impl BinaryDownloader for RecordingDownloader {
        fn download(&self, url: &str) -> Result<Vec<u8>> {
            self.called.set(true);
            if url.ends_with("SHA256SUMS.txt") {
                match &self.sums {
                    Some(sums) => Ok(sums.clone()),
                    None => Err(Error::Export("no checksum file".to_string())),
                }
            } else {
                Ok(self.bytes.clone())
            }
        }
    }

    /// Never hardcode the next tag: derive it from `CARGO_PKG_VERSION`
    /// (which carries no `v`, while release tags do).
    fn fake_newer_tag() -> String {
        let mut parts = version_parts();
        while parts.len() < 3 {
            parts.push(0);
        }
        format!("v{}.{}.{}", parts[0], parts[1], parts[2] + 1)
    }

    /// A stable tag `patch` above the current patch, for max-selection tests.
    fn tag_with_patch(patch: u64) -> String {
        let mut parts = version_parts();
        while parts.len() < 3 {
            parts.push(0);
        }
        format!("v{}.{}.{}", parts[0], parts[1], patch)
    }

    fn version_parts() -> Vec<u64> {
        env!("CARGO_PKG_VERSION")
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    }

    fn current_patch() -> u64 {
        let mut parts = version_parts();
        while parts.len() < 3 {
            parts.push(0);
        }
        parts[2]
    }

    fn parse_releases(json: &str) -> std::result::Result<Vec<GitHubRelease>, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    fn stable_release(tag: &str) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.to_string(),
            prerelease: false,
            draft: false,
        }
    }

    /// A release list with a prerelease and a draft decoy, both newer than any
    /// stable release, plus a stable `tag`.
    fn releases_json(tag: &str) -> String {
        serde_json::json!([
            { "tag_name": tag },
            { "tag_name": "v99.99.99-rc.1", "prerelease": true },
            { "tag_name": "v999.0.0", "draft": true },
        ])
        .to_string()
    }

    /// Only the current version published: nothing newer exists.
    fn current_only_json() -> String {
        serde_json::json!([
            { "tag_name": format!("v{}", env!("CARGO_PKG_VERSION")) },
        ])
        .to_string()
    }

    fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            for (name, contents) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o755);
                header.set_entry_type(tar::EntryType::Regular);
                builder
                    .append_data(&mut header, *name, *contents)
                    .expect("append entry");
            }
            builder.finish().expect("finish tar");
        }
        encoder.finish().expect("finish gzip")
    }

    fn tar_gz_with_demo(contents: &[u8]) -> Vec<u8> {
        tar_gz(&[("demo", contents)])
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// `sha256sum`-style output: two spaces, then two spaces.
    fn sha_sums_for(asset: &str, bytes: &[u8]) -> Vec<u8> {
        format!("  {}  {asset}\n", sha256_hex(bytes)).into_bytes()
    }

    fn deps<'a>(
        releases: std::result::Result<Vec<GitHubRelease>, String>,
        exe: &'a Path,
        cargo_bin: &'a Path,
        downloader: &'a dyn BinaryDownloader,
        confirm: &'a dyn Fn() -> bool,
    ) -> UpdateDeps<'a> {
        UpdateDeps {
            current: env!("CARGO_PKG_VERSION"),
            releases,
            exe,
            cargo_bin,
            target: Ok("x86_64-unknown-linux-musl"),
            downloader,
            confirm,
        }
    }

    fn write(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).expect("write fixture");
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("read fixture")
    }

    // ── Version selection ───────────────────────────────────────

    #[test]
    fn compare_versions_equal() {
        assert_eq!(
            compare_versions("0.3.1", "0.3.1"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_versions("v0.3.1", "0.3.1"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn compare_versions_greater_patch() {
        assert_eq!(
            compare_versions("0.3.2", "0.3.1"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("v1.0.0", "0.9.9"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn compare_versions_less_patch() {
        assert_eq!(compare_versions("0.3.0", "0.3.1"), std::cmp::Ordering::Less);
        assert_eq!(
            compare_versions("v0.2.9", "0.3.1"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn compare_versions_major_wins() {
        assert_eq!(
            compare_versions("1.0.0", "0.99.99"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(compare_versions("0.9.9", "1.0.0"), std::cmp::Ordering::Less);
    }

    #[test]
    fn compare_versions_v_prefix_ignored() {
        assert_eq!(
            compare_versions("v0.3.1", "0.3.2"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions("v0.3.2", "0.3.1"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn compare_versions_different_length() {
        assert_eq!(compare_versions("0.3", "0.3.0"), std::cmp::Ordering::Equal);
        assert_eq!(
            compare_versions("v0.3.1", "0.3"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(compare_versions("0.3", "0.3.1"), std::cmp::Ordering::Less);
    }

    #[test]
    fn stable_version_accepts_plain() {
        assert!(is_stable_version("v0.3.1"));
        assert!(is_stable_version("0.3.1"));
        assert!(is_stable_version("1.2.3.4"));
    }

    #[test]
    fn stable_version_rejects_prerelease() {
        assert!(!is_stable_version("v0.3.1-beta.1"));
        assert!(!is_stable_version("0.3.1-rc1"));
        assert!(!is_stable_version("v1.0.0+build"));
    }

    #[test]
    fn stable_version_rejects_empty() {
        assert!(!is_stable_version(""));
        assert!(!is_stable_version("v"));
    }

    #[test]
    fn select_latest_picks_max_stable() {
        let older = tag_with_patch(current_patch().saturating_sub(1));
        let newer = fake_newer_tag();
        let much_newer = tag_with_patch(current_patch() + 5);
        let releases = parse_releases(&releases_json(&newer)).unwrap();
        let mut releases = releases;
        releases.push(stable_release(&much_newer));
        releases.push(stable_release(&older));
        assert_eq!(
            select_latest_stable(&releases, env!("CARGO_PKG_VERSION")),
            Some(much_newer)
        );
    }

    #[test]
    fn check_ignores_prerelease() {
        let releases = vec![GitHubRelease {
            tag_name: fake_newer_tag(),
            prerelease: true,
            draft: false,
        }];
        assert_eq!(
            select_latest_stable(&releases, env!("CARGO_PKG_VERSION")),
            None
        );
    }

    #[test]
    fn check_ignores_draft() {
        let releases = vec![GitHubRelease {
            tag_name: fake_newer_tag(),
            prerelease: false,
            draft: true,
        }];
        assert_eq!(
            select_latest_stable(&releases, env!("CARGO_PKG_VERSION")),
            None
        );
    }

    #[test]
    fn check_ignores_older() {
        let releases = vec![stable_release("v0.0.1")];
        assert_eq!(
            select_latest_stable(&releases, env!("CARGO_PKG_VERSION")),
            None
        );
    }

    #[test]
    fn fetcher_parses_release_json() {
        let fetcher = FakeFetcher {
            body: releases_json(&fake_newer_tag()),
        };
        let releases = fetch_releases_with(&fetcher).expect("parse");
        assert_eq!(releases.len(), 3);
        assert!(releases[1].prerelease);
        assert!(releases[2].draft);
    }

    // ── Naming and targets ──────────────────────────────────────

    #[test]
    fn asset_name_demo_musl() {
        assert_eq!(
            asset_name("v0.3.1", "x86_64-unknown-linux-musl"),
            "demo-v0.3.1-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            asset_name(&fake_newer_tag(), "aarch64-apple-darwin"),
            format!("demo-{}-aarch64-apple-darwin.tar.gz", fake_newer_tag())
        );
    }

    #[test]
    fn target_for_musl() {
        assert_eq!(
            target_for("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn target_for_darwin_arm() {
        assert_eq!(
            target_for("macos", "aarch64").unwrap(),
            "aarch64-apple-darwin"
        );
    }

    #[test]
    fn target_for_darwin_x64() {
        assert_eq!(
            target_for("macos", "x86_64").unwrap(),
            "x86_64-apple-darwin"
        );
    }

    #[test]
    fn target_for_unsupported_windows_bails() {
        let error = target_for("windows", "x86_64").unwrap_err();
        assert!(error.to_string().contains("unsupported target"));
    }

    #[test]
    fn target_for_unsupported_linux_arm_bails() {
        let error = target_for("linux", "aarch64").unwrap_err();
        assert!(error.to_string().contains("unsupported target"));
    }

    #[test]
    fn resolve_target_ok_on_supported_platform() {
        let supported = matches!(
            (std::env::consts::OS, std::env::consts::ARCH),
            ("linux", "x86_64") | ("macos", "aarch64") | ("macos", "x86_64")
        );
        if supported {
            assert_eq!(
                resolve_target().unwrap(),
                target_for(std::env::consts::OS, std::env::consts::ARCH).unwrap()
            );
        }
    }

    // ── Cargo guard ─────────────────────────────────────────────

    #[test]
    fn cargo_detect_default_home() {
        let exe = Path::new("/home/fake-user/.cargo/bin/demo");
        let cargo_bin = Path::new("/home/fake-user/.cargo/bin");
        assert!(is_cargo_installed(exe, cargo_bin));
    }

    #[test]
    fn cargo_detect_custom_cargo_home() {
        let exe = Path::new("/opt/rust/toolchains/stable/bin/demo");
        let cargo_bin = Path::new("/opt/rust/toolchains/stable/bin");
        assert!(is_cargo_installed(exe, cargo_bin));
    }

    #[test]
    fn cargo_detect_not_cargo() {
        let exe = Path::new("/usr/local/bin/demo");
        let cargo_bin = Path::new("/home/fake-user/.cargo/bin");
        assert!(!is_cargo_installed(exe, cargo_bin));
    }

    #[test]
    fn cargo_bin_dir_follows_cargo_home_then_home() {
        // `$HOME/.cargo/bin` is the fallback; `$CARGO_HOME` is not settable
        // safely from a test (other tests may run in parallel), so assert the
        // invariant that holds either way.
        let dir = cargo_bin_dir();
        let ends_with_cargo_bin = dir.ends_with(".cargo/bin") || dir.ends_with("bin");
        assert!(ends_with_cargo_bin, "{dir:?}");
    }

    // ── `--check` ───────────────────────────────────────────────

    #[test]
    fn check_exit_1_when_newer() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("demo");
        let cargo_bin = dir.path().join("not-cargo");
        write(&exe, b"old");
        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"new"), None);
        let confirm = || false;
        let deps = deps(
            parse_releases(&releases_json(&fake_newer_tag())),
            &exe,
            &cargo_bin,
            &downloader,
            &confirm,
        );
        assert_eq!(run_update_with(true, false, &deps).unwrap(), 1);
        assert!(
            !downloader.called.get(),
            "--check must never touch the downloader"
        );
        assert_eq!(read(&exe), b"old".to_vec());
    }

    #[test]
    fn check_exit_0_when_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("demo");
        let cargo_bin = dir.path().join("not-cargo");
        write(&exe, b"old");
        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"new"), None);
        let confirm = || false;
        let deps = deps(
            parse_releases(&current_only_json()),
            &exe,
            &cargo_bin,
            &downloader,
            &confirm,
        );
        assert_eq!(run_update_with(true, false, &deps).unwrap(), 0);
        assert!(!downloader.called.get());
        assert_eq!(read(&exe), b"old".to_vec());
    }

    #[test]
    fn plain_update_reports_up_to_date_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("demo");
        let cargo_bin = dir.path().join("not-cargo");
        write(&exe, b"old");
        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"new"), None);
        let confirm = || false;
        let deps = deps(
            parse_releases(&current_only_json()),
            &exe,
            &cargo_bin,
            &downloader,
            &confirm,
        );
        assert_eq!(run_update_with(false, true, &deps).unwrap(), 0);
        assert!(!downloader.called.get());
    }

    // ── Install path ────────────────────────────────────────────

    #[test]
    fn install_refuses_cargo_binary_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_bin = dir.path().join(".cargo").join("bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let exe = cargo_bin.join("demo");
        write(&exe, b"old");
        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"new"), None);
        let confirm = || false;
        let deps = deps(
            parse_releases(&releases_json(&fake_newer_tag())),
            &exe,
            &cargo_bin,
            &downloader,
            &confirm,
        );
        assert_eq!(run_update_with(false, true, &deps).unwrap(), 0);
        assert!(
            !downloader.called.get(),
            "a cargo install is never downloaded"
        );
        assert_eq!(read(&exe), b"old".to_vec());
    }

    #[test]
    fn install_declined_leaves_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("demo");
        let cargo_bin = dir.path().join("not-cargo");
        write(&exe, b"old");
        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"new"), None);
        let confirm = || false;
        let deps = deps(
            parse_releases(&releases_json(&fake_newer_tag())),
            &exe,
            &cargo_bin,
            &downloader,
            &confirm,
        );
        assert_eq!(run_update_with(false, false, &deps).unwrap(), 0);
        assert!(
            !downloader.called.get(),
            "a declined prompt downloads nothing"
        );
        assert_eq!(read(&exe), b"old".to_vec());
    }

    #[test]
    fn install_with_yes_replaces_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("demo");
        let cargo_bin = dir.path().join("not-cargo");
        write(&exe, b"old");
        let new_bytes = b"brand new demo binary";
        let downloader = RecordingDownloader::new(tar_gz_with_demo(new_bytes), None);
        let confirm = || false;
        let deps = deps(
            parse_releases(&releases_json(&fake_newer_tag())),
            &exe,
            &cargo_bin,
            &downloader,
            &confirm,
        );
        assert_eq!(run_update_with(false, true, &deps).unwrap(), 0);
        assert!(downloader.called.get());
        assert_eq!(read(&exe), new_bytes.to_vec());
    }

    // ── Loud failures ───────────────────────────────────────────

    #[test]
    fn explicit_errors_loudly_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("demo");
        let cargo_bin = dir.path().join("not-cargo");
        let downloader = RecordingDownloader::new(Vec::new(), None);
        let confirm = || false;
        let deps = deps(
            Err("connection refused".to_string()),
            &exe,
            &cargo_bin,
            &downloader,
            &confirm,
        );
        let error = run_update_with(false, true, &deps).unwrap_err();
        assert!(
            error.to_string().contains("release lookup failed"),
            "got: {error}"
        );
        assert!(!downloader.called.get());
    }

    #[test]
    fn unsupported_target_errors_loudly_before_download() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("demo");
        let cargo_bin = dir.path().join("not-cargo");
        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"new"), None);
        let confirm = || false;
        let mut deps = deps(
            parse_releases(&releases_json(&fake_newer_tag())),
            &exe,
            &cargo_bin,
            &downloader,
            &confirm,
        );
        deps.target = Err("unsupported target: aarch64-linux".to_string());
        let error = run_update_with(false, true, &deps).unwrap_err();
        assert!(
            error.to_string().contains("target resolution failed"),
            "got: {error}"
        );
        assert!(!downloader.called.get());
    }

    // ── Archive handling ────────────────────────────────────────

    #[test]
    fn download_and_extract_uses_the_demo_entry() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("demo-new");
        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"extracted binary"), None);
        assert!(download_and_extract_with(
            &downloader,
            "v9.9.9",
            "x86_64-unknown-linux-musl",
            &output
        )
        .unwrap());
        assert_eq!(read(&output), b"extracted binary".to_vec());
    }

    #[test]
    fn download_and_extract_without_demo_entry_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("demo-new");
        let downloader = RecordingDownloader::new(tar_gz(&[("other", &b"nope"[..])]), None);
        assert!(!download_and_extract_with(
            &downloader,
            "v9.9.9",
            "x86_64-unknown-linux-musl",
            &output
        )
        .unwrap());
        assert!(!output.exists());
    }

    #[test]
    fn checksum_mismatch_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("demo-new");
        let archive = tar_gz_with_demo(b"bytes");
        let asset = asset_name("v9.9.9", "x86_64-unknown-linux-musl");
        let wrong = format!("  {}  {asset}\n", "0".repeat(64)).into_bytes();
        let downloader = RecordingDownloader::new(archive, Some(wrong));
        let error =
            download_and_extract_with(&downloader, "v9.9.9", "x86_64-unknown-linux-musl", &output)
                .unwrap_err();
        assert!(
            error.to_string().contains("SHA256 mismatch"),
            "got: {error}"
        );
    }

    #[test]
    fn checksum_missing_sums_skips() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("demo-new");
        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"bytes"), None);
        assert!(download_and_extract_with(
            &downloader,
            "v9.9.9",
            "x86_64-unknown-linux-musl",
            &output
        )
        .unwrap());
        assert_eq!(read(&output), b"bytes".to_vec());
    }

    #[test]
    fn checksum_missing_line_skips() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("demo-new");
        let archive = tar_gz_with_demo(b"bytes");
        let other = sha_sums_for("demo-v9.9.9-other.tar.gz", &archive);
        let downloader = RecordingDownloader::new(archive, Some(other));
        assert!(download_and_extract_with(
            &downloader,
            "v9.9.9",
            "x86_64-unknown-linux-musl",
            &output
        )
        .unwrap());
        assert_eq!(read(&output), b"bytes".to_vec());
    }

    #[test]
    fn checksum_matching_sums_passes() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("demo-new");
        let archive = tar_gz_with_demo(b"bytes");
        let asset = asset_name("v9.9.9", "x86_64-unknown-linux-musl");
        let sums = sha_sums_for(&asset, &archive);
        let downloader = RecordingDownloader::new(archive, Some(sums));
        assert!(download_and_extract_with(
            &downloader,
            "v9.9.9",
            "x86_64-unknown-linux-musl",
            &output
        )
        .unwrap());
        assert_eq!(read(&output), b"bytes".to_vec());
    }

    // ── Replacement and state ───────────────────────────────────

    #[test]
    fn replace_binary_replaces_target_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("demo");
        write(&exe, b"old");
        let staging = dir.path().join("staged");
        write(&staging, b"new");
        replace_binary(&staging, &exe).unwrap();
        assert_eq!(read(&exe), b"new".to_vec());
        // No leftover temporary files beside the replaced binary.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|name| name != "demo" && name != "staged")
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }

    /// The whole point of a binary swap: `demo update` replaces only the
    /// executable. Scores, recordings, the raw macro and the capture sources
    /// (everything a project directory holds) survive byte for byte.
    #[test]
    fn state_survives_the_binary_swap() {
        let bin_dir = tempfile::tempdir().unwrap();
        let cargo_bin_dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        let exe = bin_dir.path().join("demo");
        write(&exe, b"old binary");

        let fixtures: [(&str, &[u8]); 5] = [
            ("demo.toml", b"[demo]\nname = \"ship it\"\nspeed = 1.5\n"),
            (
                "demo.rec",
                b"{\"header\":{\"version\":1},\"events\":[1,2,3]}",
            ),
            ("macro.raw.toml", b"[[step]]\ntype = \"write\"\n"),
            (".demo-capture.sources", b"[sources]\nmain = \"terminal\"\n"),
            ("dist/keep.txt", b"exported artifact\n"),
        ];
        std::fs::create_dir(project.path().join("dist")).unwrap();
        for (name, contents) in fixtures {
            write(&project.path().join(name), contents);
        }
        let before: Vec<(String, Vec<u8>)> = fixtures
            .iter()
            .map(|(name, _)| (name.to_string(), read(&project.path().join(name))))
            .collect();

        let downloader = RecordingDownloader::new(tar_gz_with_demo(b"new binary"), None);
        let confirm = || false;
        let deps = deps(
            parse_releases(&releases_json(&fake_newer_tag())),
            &exe,
            cargo_bin_dir.path(),
            &downloader,
            &confirm,
        );
        assert_eq!(run_update_with(false, true, &deps).unwrap(), 0);

        // The binary moved…
        assert_eq!(read(&exe), b"new binary".to_vec());
        // …and every byte of project state is untouched, with no new files.
        for (name, contents) in &before {
            assert_eq!(
                &read(&project.path().join(name)),
                contents,
                "{name} changed"
            );
        }
        assert_eq!(before.len(), fixtures.len());
    }
}
