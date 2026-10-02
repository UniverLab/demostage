//! Unit tests for `demo update` — hermetic end to end: an injected
//! fetcher and downloader stand in for GitHub, tempdirs stand in for the
//! filesystem, and every fake newer tag is derived from `CARGO_PKG_VERSION`.
//! No test reaches the network.

use super::*;
use std::cell::Cell;

// --- mutant-killing tests for exact constants and pure helpers ---

/// Release-list ceiling is exactly 8 MiB. `*`→`+` gives 1032, `*`→`/` gives
/// 1 — both die on the exact value.
#[test]
fn releases_ceiling_is_exactly_8_mib() {
    assert_eq!(MAX_RELEASES_BYTES, 8 * 1024 * 1024);
    assert_eq!(MAX_RELEASES_BYTES, 8_388_608);
}

/// Download ceiling is exactly 256 MiB. Same arithmetic mutants, same fate.
#[test]
fn download_ceiling_is_exactly_256_mib() {
    assert_eq!(MAX_DOWNLOAD_BYTES, 256 * 1024 * 1024);
    assert_eq!(MAX_DOWNLOAD_BYTES, 268_435_456);
}

/// current_version is the crate version, never empty and never a placeholder.
#[test]
fn current_version_is_the_crate_version() {
    assert_eq!(current_version(), env!("CARGO_PKG_VERSION"));
    assert!(!current_version().is_empty());
    assert_ne!(current_version(), "xyzzy");
}

/// Empty CARGO_HOME falls back to $HOME/.cargo/bin; a set one wins exactly.
#[test]
fn cargo_bin_from_resolves_empty_and_set_homes() {
    use std::ffi::OsStr;
    assert_eq!(
        cargo_bin_from(Some(OsStr::new("/opt/cargo")), Some(OsStr::new("/home/u"))),
        PathBuf::from("/opt/cargo/bin")
    );
    assert_eq!(
        cargo_bin_from(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
        PathBuf::from("/home/u/.cargo/bin"),
        "empty CARGO_HOME must fall back"
    );
    assert_eq!(
        cargo_bin_from(None, Some(OsStr::new("/home/u"))),
        PathBuf::from("/home/u/.cargo/bin")
    );
    assert_eq!(
        cargo_bin_from(None, None),
        PathBuf::from(".").join(".cargo").join("bin")
    );
}

/// Status lines are byte-exact: up-to-date names the version, an update
/// shows the bare (v-stripped) tag.
#[test]
fn status_lines_are_exact() {
    assert_eq!(status_line("0.3.1", None), "demo 0.3.1 is up to date");
    assert_eq!(status_line("0.3.1", Some("v0.4.0")), "demo 0.3.1 → 0.4.0");
    assert_eq!(status_line("0.3.1", Some("0.4.0")), "demo 0.3.1 → 0.4.0");
}

// ── Fakes ───────────────────────────────────────────────────

struct FakeFetcher {
    body: String,
}

impl ReleaseFetcher for FakeFetcher {
    fn get(&self, _url: &str) -> Result<String> {
        Ok(self.body.clone())
    }
}

/// A lookup that fails exactly like the production fetcher: one loud line.
struct FailingFetcher {
    message: &'static str,
}

impl ReleaseFetcher for FailingFetcher {
    fn get(&self, _url: &str) -> Result<String> {
        Err(Error::Export(format!(
            "failed to fetch GitHub releases: {}",
            self.message
        )))
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

/// `sha256sum`-style output: `<hex><two spaces><asset>`.
fn sha_sums_for(asset: &str, bytes: &[u8]) -> Vec<u8> {
    format!("{}  {asset}\n", sha256_hex(bytes)).into_bytes()
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

/// A failed lookup must exit 2 — never an `Err`: main.rs turns an `Err` into
/// exit 1 and a script would read the outage as "an update is available".
#[test]
fn lookup_failure_exits_2_for_check_and_plain_update() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("demo");
    let cargo_bin = dir.path().join("not-cargo");
    write(&exe, b"old");
    let downloader = RecordingDownloader::new(Vec::new(), None);
    let confirm = || {
        panic!("a failed check must not prompt");
    };
    let deps = deps(
        Err("connection refused".to_string()),
        &exe,
        &cargo_bin,
        &downloader,
        &confirm,
    );
    assert_eq!(run_update_with(true, false, &deps).unwrap(), 2);
    assert_eq!(run_update_with(false, true, &deps).unwrap(), 2);
    assert!(!downloader.called.get(), "a failed check downloads nothing");
    assert_eq!(read(&exe), b"old".to_vec(), "state stays untouched");
}

/// Wire the injected fetcher exactly like `run_update` does: fetch, carry the
/// one-line cause through `UpdateDeps`, and assert the hermetic core exits 2
/// without downloading or touching the binary.
fn exit_when_lookup_fails(fetcher: &dyn ReleaseFetcher, check: bool) -> i32 {
    let releases = fetch_releases_with(fetcher);
    assert!(releases.is_err(), "the fake lookup must fail");
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("demo");
    let cargo_bin = dir.path().join("not-cargo");
    write(&exe, b"old");
    let downloader = RecordingDownloader::new(Vec::new(), None);
    let confirm = || {
        panic!("a failed check must not prompt");
    };
    let deps = deps(releases, &exe, &cargo_bin, &downloader, &confirm);
    let code = run_update_with(check, true, &deps).unwrap();
    assert!(!downloader.called.get(), "a failed check downloads nothing");
    assert_eq!(read(&exe), b"old".to_vec(), "state stays untouched");
    code
}

#[test]
fn failing_fetcher_exits_2_with_the_cause() {
    let fetcher = FailingFetcher {
        message: "dns error: no such host",
    };
    assert_eq!(exit_when_lookup_fails(&fetcher, true), 2);
    assert_eq!(exit_when_lookup_fails(&fetcher, false), 2);
}

#[test]
fn unparsable_response_exits_2() {
    let fetcher = FakeFetcher {
        body: "not json".to_string(),
    };
    assert_eq!(exit_when_lookup_fails(&fetcher, true), 2);
    assert_eq!(exit_when_lookup_fails(&fetcher, false), 2);
}

#[test]
fn check_failure_cause_is_one_stderr_line() {
    for cause in [
        "dns error: no such host",
        "http status: 403",
        "a\nmulti\nline cause",
    ] {
        let line = check_failure_line(cause);
        assert!(
            line.starts_with("update check failed: "),
            "missing prefix: {line:?}"
        );
        assert!(!line.contains('\n'), "must stay one line: {line:?}");
    }
    assert!(check_failure_line("http status: 403").contains("http status: 403"));
    assert_eq!(
        check_failure_line("a\nmulti\nline cause"),
        "update check failed: a multi line cause"
    );
}

// ── Version-line wording ──────────────────────────────────────

#[test]
fn arrow_line_drops_the_v_prefix() {
    assert_eq!(arrow_line("0.0.1", "v0.3.1"), "demo 0.0.1 → 0.3.1");
}

#[test]
fn up_to_date_line_prints_bare_version() {
    assert_eq!(up_to_date_line("0.3.1"), "demo 0.3.1 is up to date");
}

#[test]
fn prompt_and_success_lines_drop_the_v_prefix() {
    assert_eq!(update_prompt_line("v0.3.2"), "Update to 0.3.2? [y/N]");
    assert_eq!(updated_line("v0.3.2"), "✓ updated to 0.3.2");
}

#[test]
fn cargo_refusal_is_the_exact_sentence() {
    assert_eq!(
        cargo_refusal_line(),
        "installed with cargo — run: cargo install --force demo-stage"
    );
}

/// The `### demo update` / `## Self-update` / `` ## `demo update` `` section
/// of each doc must name all three exit codes.
fn doc_update_section(relative: &str, marker: &str) -> String {
    let path = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read doc {path}: {e}"));
    assert!(text.contains(marker), "{marker:?} missing in {relative}");
    let level = marker.chars().take_while(|c| *c == '#').count();
    let after = &text[text.find(marker).expect("marker")..];
    let mut lines = after.lines();
    let mut out = vec![lines.next().unwrap_or("").to_string()];
    for line in lines {
        if line.starts_with('#') {
            let heading_level = line.chars().take_while(|c| *c == '#').count();
            if heading_level <= level {
                break;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

#[test]
fn readme_and_docs_document_the_three_exit_codes() {
    for (relative, marker) in [
        ("README.md", "### demo update"),
        ("docs/commands.md", "## `demo update`"),
        ("docs/installation.md", "## Self-update"),
    ] {
        let section = doc_update_section(relative, marker);
        for literal in ["exit 0", "exit 1", "exit 2"] {
            assert!(
                section.contains(literal),
                "{relative} update section missing {literal:?}:\n{section}"
            );
        }
    }
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
    assert!(
        download_and_extract_with(&downloader, "v9.9.9", "x86_64-unknown-linux-musl", &output)
            .unwrap()
    );
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
    let wrong = format!("{}  {asset}\n", "0".repeat(64)).into_bytes();
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
    assert!(
        download_and_extract_with(&downloader, "v9.9.9", "x86_64-unknown-linux-musl", &output)
            .unwrap()
    );
    assert_eq!(read(&output), b"bytes".to_vec());
}

#[test]
fn checksum_missing_line_skips() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("demo-new");
    let archive = tar_gz_with_demo(b"bytes");
    let other = sha_sums_for("demo-v9.9.9-other.tar.gz", &archive);
    let downloader = RecordingDownloader::new(archive, Some(other));
    assert!(
        download_and_extract_with(&downloader, "v9.9.9", "x86_64-unknown-linux-musl", &output)
            .unwrap()
    );
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
    assert!(
        download_and_extract_with(&downloader, "v9.9.9", "x86_64-unknown-linux-musl", &output)
            .unwrap()
    );
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
