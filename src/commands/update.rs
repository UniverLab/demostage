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
//!
//! Exit codes of `demo update [--check]`:
//!
//! | code | meaning |
//! |---|---|
//! | `0` | up to date / update installed / prompt declined / cargo refusal |
//! | `1` | `--check`: an update is available; plain `update`: a failure after a successful check |
//! | `2` | the release check could not be completed (network, DNS, TLS, HTTP ≥ 400, unparsable response); the cause is the single line on stderr — for `--check` and plain `update` alike |

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
///
/// `releases: Err(_)` means the release check could not be completed
/// (network, DNS, TLS, HTTP ≥ 400, unparsable response) — the core prints the
/// one-line cause on stderr and returns `Ok(2)`.
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
/// Exit codes: `0` = up to date (also: update installed, prompt declined, or
/// a cargo-installed binary), `1` = `--check` found an update is available
/// (plain `update`: a failure after a successful check), `2` = the release
/// check could not be completed (network, DNS, TLS, HTTP ≥ 400, unparsable
/// response) — the cause is the single line on stderr, for `--check` and
/// plain `demo update` alike.
pub fn run_update(check: bool, yes: bool) -> Result<i32> {
    let current = current_version();
    // A failed lookup is not an `Err` (main.rs would exit 1 and a script would
    // read the outage as "an update is available"): it is the "check could not
    // complete" outcome, carried as a one-line cause through `UpdateDeps`.
    let releases = fetch_releases_with(&RealFetcher);
    let latest = releases
        .as_ref()
        .ok()
        .and_then(|releases| select_latest_stable(releases, current));

    match latest {
        // An install needs the executable, cargo-guard and target facts;
        // resolve them only once a newer release is established.
        Some(latest) if !check => {
            let releases = releases.expect("a newer release implies a successful lookup");
            let exe = std::env::current_exe()
                .map_err(|e| Error::Export(format!("failed to locate demo executable: {e}")))?;
            let cargo_bin = cargo_bin_dir();
            if is_cargo_installed(&exe, &cargo_bin) {
                report_status(current, Some(&latest));
                println!("{}", cargo_refusal_line());
                return Ok(0);
            }
            let target = resolve_target()?;
            let deps = UpdateDeps {
                current,
                releases: Ok(releases),
                exe: &exe,
                cargo_bin: &cargo_bin,
                target: Ok(target),
                downloader: &RealDownloader,
                confirm: &|| {
                    inquire::Confirm::new(&update_prompt_line(&latest))
                        .with_default(false)
                        .prompt()
                        .unwrap_or(false)
                },
            };
            run_update_core(false, yes, &deps)
        }
        // `--check`, an up-to-date install, or a failed lookup: the hermetic
        // core owns every line and the exit-code contract. The executable
        // facts below are placeholders that this arm never reaches
        // (`--check` returns before the cargo guard; "up to date" and a
        // failed check return before the prompt) — and they must NOT be
        // `Path::new("")`, because `is_cargo_installed("", "")` is `true`
        // (empty starts_with empty) and would print a bogus cargo refusal if
        // the invariant were ever broken. `target: Err(...)` fails loudly if
        // it were ever read.
        _ => {
            let deps = UpdateDeps {
                current,
                releases,
                exe: Path::new("/nonexistent-demo-update-path/demo"),
                cargo_bin: Path::new("/nonexistent-demo-update-path/bin"),
                target: Err("placeholder: not reachable in this path".to_string()),
                downloader: &RealDownloader,
                confirm: &|| false,
            };
            run_update_core(check, yes, &deps)
        }
    }
}

/// Hermetic update flow used by unit tests: no network, no prompt, no setup —
/// callers provide every external fact through [`UpdateDeps`]. A failed
/// lookup (`releases: Err(_)`) is the "check could not complete" outcome:
/// one stderr line, `Ok(2)`.
pub fn run_update_with(check: bool, yes: bool, deps: &UpdateDeps<'_>) -> Result<i32> {
    run_update_core(check, yes, deps)
}

fn run_update_core(check: bool, yes: bool, deps: &UpdateDeps<'_>) -> Result<i32> {
    let releases = match deps.releases.as_ref() {
        Ok(releases) => releases,
        Err(cause) => {
            // Contract: the check could not complete — one stderr line, exit 2,
            // for `--check` and a plain update alike; nothing below runs
            // (no cargo guard, no target, no prompt, no download).
            eprintln!("{}", check_failure_line(cause));
            return Ok(2);
        }
    };
    let latest = select_latest_stable(releases, deps.current);
    report_status(deps.current, latest.as_deref());
    let Some(latest) = latest else {
        return Ok(0);
    };
    // `--check` reports availability only: exit before any local side effect.
    if check {
        return Ok(1);
    }
    // A cargo install is owned by cargo — never overwrite it.
    if is_cargo_installed(deps.exe, deps.cargo_bin) {
        println!("{}", cargo_refusal_line());
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
    println!("{}", updated_line(&latest));
    Ok(0)
}

// ── Version helpers ─────────────────────────────────────────────

/// A release tag without its leading `v`: every version line prints bare
/// (`demo 0.0.1 → 0.3.1`) to match the texforge wording. Never used for the
/// asset name or the download URL — those keep the `v`.
fn bare_version(tag: &str) -> &str {
    tag.trim_start_matches('v')
}

fn up_to_date_line(current: &str) -> String {
    format!("demo {current} is up to date")
}

fn arrow_line(current: &str, latest: &str) -> String {
    format!("demo {current} → {}", bare_version(latest))
}

/// Byte-stable across the lab; the spec pins this exact sentence.
fn cargo_refusal_line() -> &'static str {
    "installed with cargo — run: cargo install --force demo-stage"
}

fn update_prompt_line(latest: &str) -> String {
    format!("Update to {}? [y/N]", bare_version(latest))
}

fn updated_line(latest: &str) -> String {
    format!("✓ updated to {}", bare_version(latest))
}

/// What lands on stderr when the check could not complete: exactly one line
/// that names the cause (a multi-line cause would break a script parsing it).
fn check_failure_line(cause: &str) -> String {
    let flat = cause
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    format!("update check failed: {flat}")
}

/// The shared status line: current version, with the newest stable tag when
/// one is available.
fn report_status(current: &str, latest: Option<&str>) {
    println!("{}", status_line(current, latest));
}

/// Pure rendering of the shared status line, so the wording is testable
/// without capturing stdout.
fn status_line(current: &str, latest: Option<&str>) -> String {
    match latest {
        None => up_to_date_line(current),
        Some(tag) => arrow_line(current, tag),
    }
}

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
/// A failure is the one-line "check could not complete" cause carried through
/// [`UpdateDeps`], never an `Err` (exit 2, not exit 1).
fn fetch_releases_with(
    fetcher: &dyn ReleaseFetcher,
) -> std::result::Result<Vec<GitHubRelease>, String> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases");
    let body = fetcher.get(&url).map_err(|error| error.to_string())?;
    serde_json::from_str(&body).map_err(|error| format!("failed to parse releases JSON: {error}"))
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
    cargo_bin_from(
        std::env::var_os("CARGO_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Pure resolution of the cargo bin directory from its two environment
/// inputs, so the empty-`CARGO_HOME` fallback is unit-testable.
fn cargo_bin_from(cargo_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> PathBuf {
    cargo_home
        .filter(|value| !value.is_empty())
        .map(|value| PathBuf::from(value).join("bin"))
        .unwrap_or_else(|| {
            home.map(PathBuf::from)
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

#[cfg(test)]
mod tests;
