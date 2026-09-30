//! Browser pane source: drive a headless Chromium to capture a web/PDF scene as
//! RGBA keyframes. Chromium is provisioned tectonic-style — a system install is
//! used if present, otherwise `headless_chrome`'s fetcher downloads a managed
//! build on first use.
//!
//! NOTE: this path needs a real Chromium and so is NOT exercised by the test
//! suite or in the restricted dev sandbox; it is verified on a machine with
//! Chromium available (or network to fetch it).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use headless_chrome::protocol::cdp::Emulation::{MediaFeature, SetEmulatedMedia};
use headless_chrome::protocol::cdp::Page::{CaptureScreenshotFormatOption, Navigate};
use headless_chrome::{Browser, LaunchOptions, Tab};

use super::provision;
use super::stage::{scroll_offsets_with_params, ScrollParams};
use crate::error::{Error, Result};
use crate::model::{view_frames_dir, Pane, ScrollDirection, Velocity};

/// Screenshots per second of pane time for a headless web pane. One CDP
/// screenshot measured ~291 ms on this machine, so a 10 s pane at one capture
/// per output frame costs ~87 s; at this rate it costs ~12 s.
const MAX_CAPTURES_PER_SECOND: f64 = 4.0;

/// Per-pane capture stats for headless web panes, printed at export end.
pub struct BrowserCaptureReport {
    pub pane_id: String,
    pub frame_count: usize,
    pub elapsed: Duration,
}

/// Guard that removes a temporary directory on drop.
pub struct TempDirGuard(Option<PathBuf>);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Result of a headless browser capture: the scene, a guard that cleans up the
/// temporary frames directory, and optional capture stats for cost reporting.
pub struct CaptureResult {
    pub scene: AnyScene,
    pub _guard: TempDirGuard,
    pub report: Option<BrowserCaptureReport>,
}

/// Compute the absolute scroll offsets for `frames` output frames, given a page
/// of `scroll_height` pixels and a viewport of `viewport_height` pixels.
/// Returns a single-element vector `[0]` when the page is not scrollable or
/// only one frame is requested.
fn scroll_offsets(
    scroll_height: usize,
    viewport_height: usize,
    frames: usize,
    direction: ScrollDirection,
    velocity: Velocity,
) -> Vec<usize> {
    let max_offset = scroll_height.saturating_sub(viewport_height);
    scroll_offsets_with_params(max_offset, frames, direction, velocity)
}

/// Frame rate an interactive `--view` session is recorded at.
pub const VIEW_FPS: u32 = 8;

/// A captured browser scene: RGBA keyframes keyed by timeline progress in
/// `[0.0, 1.0]`. Frames between keyframes hold the latest one.
pub struct Scene {
    pub width: usize,
    pub height: usize,
    keyframes: Vec<(f64, Vec<u8>)>,
}

impl Scene {
    /// Build a scene from explicit keyframes (used by tests and the Chrome
    /// PDF fallback, which collects its shots before constructing).
    pub fn from_keyframes(width: usize, height: usize, keyframes: Vec<(f64, Vec<u8>)>) -> Self {
        Scene {
            width,
            height,
            keyframes,
        }
    }

    /// The frame to show at `progress` (the latest keyframe at or before it).
    pub fn frame_at(&self, progress: f64) -> &[u8] {
        let mut chosen = &self.keyframes[0].1;
        for (p, f) in &self.keyframes {
            if *p <= progress {
                chosen = f;
            } else {
                break;
            }
        }
        chosen
    }
}

/// A frame source backed by a directory of PNGs decoded on demand.
/// Keeps at most one frame decoded at a time; the consumer reads progress
/// monotonically, so a one-frame cache plus re-decode is sufficient.
#[derive(Debug)]
pub struct DirScene {
    width: usize,
    height: usize,
    files: Vec<PathBuf>,
    progresses: Vec<f64>,
    cached_index: Option<usize>,
    cached_frame: Vec<u8>,
    #[cfg(test)]
    decode_count: usize,
}

impl DirScene {
    fn new(width: usize, height: usize, files: Vec<PathBuf>, progresses: Vec<f64>) -> Self {
        Self {
            width,
            height,
            files,
            progresses,
            cached_index: None,
            cached_frame: Vec::new(),
            #[cfg(test)]
            decode_count: 0,
        }
    }

    fn frame_at(&mut self, progress: f64) -> &[u8] {
        let idx = self.pick_index(progress);
        if self.cached_index != Some(idx) {
            let bytes = std::fs::read(&self.files[idx])
                .unwrap_or_else(|e| panic!("read {}: {e}", self.files[idx].display()));
            let rgba = png_to_rgba(&bytes, self.width, self.height)
                .unwrap_or_else(|e| panic!("decode {}: {e}", self.files[idx].display()));
            self.cached_index = Some(idx);
            self.cached_frame = rgba;
            #[cfg(test)]
            {
                self.decode_count += 1;
            }
        }
        &self.cached_frame
    }

    fn pick_index(&self, progress: f64) -> usize {
        let mut chosen = 0;
        for (i, p) in self.progresses.iter().enumerate() {
            if *p <= progress {
                chosen = i;
            } else {
                break;
            }
        }
        chosen
    }

    #[cfg(test)]
    fn decode_count(&self) -> usize {
        self.decode_count
    }
}

/// A browser scene backed by either in-memory keyframes, a directory of
/// PNGs decoded on demand, or a native PDF scene that computes viewport
/// slices on demand.
pub enum AnyScene {
    Keyframe(Scene),
    Directory(DirScene),
    Pdf(super::pdf::PdfScene),
}

impl AnyScene {
    /// Seconds this scene needs on screen to show everything it has, at or below
    /// its speed cap. Only the native PDF path asks for time; the others are
    /// content-agnostic and take whatever window they are given.
    pub fn needed_seconds(&self) -> f64 {
        match self {
            Self::Pdf(p) => p.needed_seconds(),
            _ => 0.0,
        }
    }

    /// Tell a scene the real number of output frames its pane is on screen for.
    /// Only the PDF path cares; the others already map progress over whatever
    /// window they are given.
    pub fn set_window_frames(&mut self, frames: usize) {
        if let Self::Pdf(p) = self {
            p.set_window_frames(frames);
        }
    }

    pub fn width(&self) -> usize {
        match self {
            Self::Keyframe(s) => s.width,
            Self::Directory(d) => d.width,
            Self::Pdf(p) => p.width(),
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Self::Keyframe(s) => s.height,
            Self::Directory(d) => d.height,
            Self::Pdf(p) => p.height(),
        }
    }

    pub fn frame_at(&mut self, progress: f64) -> &[u8] {
        match self {
            Self::Keyframe(s) => s.frame_at(progress),
            Self::Directory(d) => d.frame_at(progress),
            Self::Pdf(p) => p.frame_at(progress),
        }
    }
}

impl From<Scene> for AnyScene {
    fn from(s: Scene) -> Self {
        Self::Keyframe(s)
    }
}

/// Whether a pane URL points at a PDF (case-insensitive). Pure so the
/// gate is unit-testable without rendering anything.
fn is_pdf_url(url: &str) -> bool {
    url.to_lowercase().ends_with(".pdf")
}

/// Progress of the i-th Chrome-viewer keyframe: the first shot is the top
/// (0.0); scrolling shots span 0.5..=1.0. Pure so the spacing is testable
/// without a live browser.
fn chrome_pdf_progress(i: usize, scroll_keyframes: usize) -> f64 {
    0.5 + 0.5 * ((i + 1) as f64 / scroll_keyframes as f64)
}

/// Render a native PDF scene (hayro) for `url`, when it *is* a PDF.
/// `None` for non-PDF URLs and when the native render fails — the caller then
/// falls back to Chrome's own viewer, reporting the failure.
fn native_pdf_scene(
    url: &str,
    w: usize,
    h: usize,
    output_frames: usize,
    fps: f64,
    pan: Option<ScrollParams>,
    effective_speed: f64,
) -> Option<CaptureResult> {
    if !is_pdf_url(url) {
        return None;
    }
    match local_file_path(url)
        .and_then(|p| super::pdf::capture_scene(&p, w, h, output_frames, fps, pan, effective_speed))
    {
        Ok(scene) => Some(CaptureResult {
            scene: AnyScene::Pdf(scene),
            _guard: TempDirGuard(None),
            report: None,
        }),
        Err(e) => {
            eprintln!("demo: native PDF render failed ({e}), falling back to Chrome viewer");
            None
        }
    }
}

/// Open a tab sized to the pane and force the viewport to its exact
/// dimensions, applying the pane's theme preference.
fn open_tab_for_pane(browser: &Browser, pane: &Pane, w: usize, h: usize) -> Result<Arc<Tab>> {
    let tab = browser
        .new_tab()
        .map_err(|e| Error::Export(format!("open tab: {e}")))?;
    let _ = tab.set_bounds(headless_chrome::types::Bounds::Normal {
        left: Some(0),
        top: Some(0),
        width: Some(w as f64),
        height: Some(h as f64),
    });
    emulate_theme(&tab, pane.theme.as_deref());
    Ok(tab)
}

/// Fallback for PDFs when the native hayro render fails: drive Chrome's own
/// PDF viewer, paging through `scroll_keyframes` captures. The viewer may
/// still paint blank in headless mode on some systems.
fn capture_chrome_pdf_scene(
    tab: &Arc<Tab>,
    url: &str,
    w: usize,
    h: usize,
    scroll_keyframes: usize,
) -> Result<CaptureResult> {
    let _ = tab.call_method(Navigate {
        url: url.to_string(),
        referrer: None,
        transition_Type: None,
        frame_id: None,
        referrer_policy: None,
    });
    std::thread::sleep(Duration::from_millis(3000));

    let mut keyframes = vec![(0.0, shot(tab, w, h)?)];
    for i in 0..scroll_keyframes {
        let _ = tab.evaluate(
            "window.scrollBy(0, Math.round(window.innerHeight * 0.85));",
            false,
        );
        let _ = tab.press_key("PageDown");
        std::thread::sleep(Duration::from_millis(350));
        let progress = chrome_pdf_progress(i, scroll_keyframes);
        keyframes.push((progress, shot(tab, w, h)?));
    }
    Ok(CaptureResult {
        scene: AnyScene::Keyframe(Scene {
            width: w,
            height: h,
            keyframes,
        }),
        _guard: TempDirGuard(None),
        report: None,
    })
}

/// Render a browser pane's `url`, emitting a scene that covers `output_frames`
/// of output. For headless web panes, captures one screenshot per output frame
/// with absolute scroll positions, writing PNGs to a temporary directory that
/// is cleaned up when the returned guard is dropped. `should_scroll` controls
/// whether a native PDF pane pans through its document. `direction` and
/// `velocity` control the scroll behavior. `effective_speed` is the export
/// speed multiplier (1.0 for panes with `ignore_speed`), threaded through to
/// the PDF pan path.
pub fn capture(
    pane: &Pane,
    scroll_keyframes: usize,
    output_frames: usize,
    fps: f64,
    pan: Option<ScrollParams>,
    effective_speed: f64,
) -> Result<CaptureResult> {
    let url = pane
        .url
        .as_deref()
        .ok_or_else(|| Error::Export(format!("browser pane '{}' has no url", pane.id)))?;
    let (w, h) = (pane.width as usize, pane.height as usize);

    // A `--view` scene is already recorded — play its frames back, no Chromium.
    if let Some(dir) = view_frames_dir(url) {
        let scene = load_frames(Path::new(dir), w, h).map(AnyScene::Directory)?;
        return Ok(CaptureResult {
            scene,
            _guard: TempDirGuard(None),
            report: None,
        });
    }

    let url = crate::paths::resolve_browser_url(url)?;

    // PDFs render natively (hayro) — no Chromium launch, no blank-viewer risk,
    // and the scene starts instantly. Chrome's viewer is only a fallback.
    if let Some(scene) = native_pdf_scene(&url, w, h, output_frames, fps, pan, effective_speed) {
        return Ok(scene);
    }

    if provision::find_chromium().is_none() {
        eprintln!("demo: Chromium not found — fetching a managed copy (one time)…");
    }

    let options = LaunchOptions::default_builder()
        .headless(true)
        // Headless WSL/CI hosts usually run without a working Chromium sandbox,
        // where it refuses to start — disable it.
        .sandbox(false)
        .window_size(Some((pane.width, pane.height)))
        .build()
        .map_err(|e| Error::Export(format!("chromium launch options: {e}")))?;
    let browser = Browser::new(options).map_err(|e| {
        Error::Export(format!(
            "launch chromium: {e}\n  \
             • If this says \"no available ports … for debugging\", you have the \
             *snap* Chromium (Ubuntu's default). Its sandbox blocks the remote-debug \
             port, so it can't drive headless. Install the non-snap Google Chrome \
             instead:\n      \
             wget https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb\n      \
             sudo apt install ./google-chrome-stable_current_amd64.deb\n    \
             (it's picked automatically over the snap).\n  \
             • Or run the export on a host with a browser — the `.rec` is portable, \
             so `demo export demo.rec` from Windows works.\n  \
             • Or capture the scene with `demo open --view` (records frames up front, \
             so export needs no browser)."
        ))
    })?;

    let tab = open_tab_for_pane(&browser, pane, w, h)?;

    let is_pdf = url.to_lowercase().ends_with(".pdf");

    if is_pdf {
        capture_chrome_pdf_scene(&tab, &url, w, h, scroll_keyframes)
    } else {
        tab.navigate_to(&url)
            .and_then(|t| t.wait_until_navigated())
            .map_err(|e| Error::Export(format!("navigate to {url}: {e}")))?;
        // Give the page a moment to paint.
        std::thread::sleep(Duration::from_millis(900));

        capture_web_pane(&tab, pane, w, h, output_frames, fps, pan)
    }
}

/// Capture frames to a directory using the given offsets and screenshot function.
/// Each offset triggers a screenshot call; the resulting PNG is written to disk.
/// Returns the list of file paths on success, or an error (the temp directory
/// is cleaned up by the caller's guard).
fn capture_frames_to_dir(
    offsets: &[usize],
    target_dir: &Path,
    screenshot: impl Fn(usize) -> Result<Vec<u8>>,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::with_capacity(offsets.len());
    for (i, &offset) in offsets.iter().enumerate() {
        let png = screenshot(i)
            .map_err(|e| Error::Export(format!("screenshot frame {i} at offset {offset}: {e}")))?;
        let path = target_dir.join(format!("{:04}.png", i + 1));
        std::fs::write(&path, &png).map_err(|e| Error::Export(format!("write frame {i}: {e}")))?;
        files.push(path);
    }
    Ok(files)
}

/// Capture a web pane as one PNG per output frame, with absolute scroll
/// positions derived from the page's own dimensions. Frames land in a temp
/// directory and the scene reads them back on demand.
fn capture_web_pane(
    tab: &Arc<Tab>,
    pane: &Pane,
    w: usize,
    h: usize,
    output_frames: usize,
    fps: f64,
    pan: Option<ScrollParams>,
) -> Result<CaptureResult> {
    // A browser frame costs a CDP screenshot round-trip — measured at ~291 ms,
    // four orders of magnitude more than a PDF frame, which is a memcpy. So this
    // path captures at a bounded rate and holds each frame, while the PDF path
    // renders one per output frame. Same feature, opposite economics.
    let frames = capture_budget(output_frames.max(1), fps);

    let page = match pan {
        Some(_) => Some((
            js_usize(tab, "document.documentElement.scrollHeight")?,
            js_usize(tab, "window.innerHeight")?,
        )),
        None => None,
    };
    let offsets = web_pane_offsets(page, frames, pan.map(|p| (p.direction, p.velocity)));
    let actual_frames = offsets.len();

    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_dir =
        std::env::temp_dir().join(format!("demostage_browser_{}_{}", std::process::id(), id));
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| Error::Export(format!("create temp frames dir: {e}")))?;
    let guard = TempDirGuard(Some(tmp_dir.clone()));

    let start = Instant::now();
    let url_for_err = pane.url.clone();
    let files = capture_frames_to_dir(&offsets, &tmp_dir, |i| {
        let js = format!("window.scrollTo(0, {});", offsets[i]);
        let _ = tab.evaluate(&js, false);
        tab.capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
            .map_err(|e| {
                Error::Export(format!(
                    "frame {} for {}: {e}",
                    i,
                    url_for_err.as_deref().unwrap_or("?")
                ))
            })
    })?;
    let elapsed = start.elapsed();

    let progresses: Vec<f64> = if actual_frames > 1 {
        (0..actual_frames)
            .map(|i| i as f64 / (actual_frames - 1) as f64)
            .collect()
    } else {
        vec![0.0]
    };

    let scene = AnyScene::Directory(DirScene::new(w, h, files, progresses));
    let report = BrowserCaptureReport {
        pane_id: pane.id.clone(),
        frame_count: actual_frames,
        elapsed,
    };
    Ok(CaptureResult {
        scene,
        _guard: guard,
        report: Some(report),
    })
}

/// How many screenshots a web pane is worth: at most [`MAX_CAPTURES_PER_SECOND`]
/// per second of pane time, never more than one per output frame.
///
/// The scene holds each captured frame until the next, so a lower budget reads
/// as a coarser scroll — not as a shorter one.
fn capture_budget(output_frames: usize, fps: f64) -> usize {
    let seconds = output_frames as f64 / fps.max(1.0);
    let capped = (seconds * MAX_CAPTURES_PER_SECOND).ceil() as usize;
    capped.clamp(1, output_frames)
}

/// Frame offsets for a web pane: the scroll ramp when a `scroll` step asked for
/// one, a single still otherwise.
///
/// A pane nobody scrolled is a still — one screenshot held for its whole window.
/// Capturing one per output frame would spend a screenshot apiece photographing
/// the same pixels, and any motion the page makes on its own (a spinner, a lazy
/// image, a hover) would leak into a demo that never asked for it.
fn web_pane_offsets(
    page: Option<(usize, usize)>,
    frames: usize,
    scroll: Option<(ScrollDirection, Velocity)>,
) -> Vec<usize> {
    match (page, scroll) {
        (Some((scroll_height, viewport_height)), Some((direction, velocity))) => {
            scroll_offsets(scroll_height, viewport_height, frames, direction, velocity)
        }
        _ => vec![0],
    }
}

/// Evaluate a JS expression that returns a number and extract it as `usize`.
fn js_usize(tab: &Arc<Tab>, expr: &str) -> Result<usize> {
    let result = tab
        .evaluate(expr, false)
        .map_err(|e| Error::Export(format!("evaluate '{expr}': {e}")))?;
    result
        .value
        .and_then(|v| v.as_f64().map(|f| f as usize))
        .ok_or_else(|| Error::Export(format!("'{expr}' did not return a number")))
}

/// Record an **interactive** browsing session: open a real (headed) browser at
/// `url`, let the user navigate, and save a PNG frame every `1/`[`VIEW_FPS`] until
/// they close the window. Frames are written `0001.png`, `0002.png`, … into
/// `out_dir`; returns how many were captured. Used by `demo open --view`.
pub fn record_view(
    url: &str,
    width: u32,
    height: u32,
    theme: Option<&str>,
    out_dir: &Path,
) -> Result<usize> {
    std::fs::create_dir_all(out_dir).map_err(|e| Error::io(out_dir, e))?;
    let url = crate::paths::resolve_browser_url(url)?;

    if provision::find_chromium().is_none() {
        eprintln!("demo: Chromium not found — fetching a managed copy (one time)…");
    }

    let options = LaunchOptions::default_builder()
        .headless(false)
        .sandbox(false)
        .window_size(Some((width, height)))
        .build()
        .map_err(|e| Error::Export(format!("chromium launch options: {e}")))?;
    let browser = Browser::new(options).map_err(|e| {
        Error::Export(format!(
            "launch a visible browser: {e}\n  \
             • `--view` opens a real (headed) browser, so it needs a graphical \
             display — on WSL that means WSLg (a recent Windows 11).\n  \
             • If it says \"no available ports … for debugging\", you have the \
             *snap* Chromium — its sandbox blocks the debug port `--view` drives it \
             through. Install the non-snap Google Chrome:\n      \
             wget https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb\n      \
             sudo apt install ./google-chrome-stable_current_amd64.deb"
        ))
    })?;

    let tab = browser
        .new_tab()
        .map_err(|e| Error::Export(format!("open tab: {e}")))?;
    // Force the viewport to the exact requested dimensions (matches `capture`).
    let _ = tab.set_bounds(headless_chrome::types::Bounds::Normal {
        left: Some(0),
        top: Some(0),
        width: Some(width as f64),
        height: Some(height as f64),
    });
    emulate_theme(&tab, theme);

    let is_pdf = url.to_lowercase().ends_with(".pdf");

    if is_pdf {
        let _ = tab.call_method(Navigate {
            url: url.to_string(),
            referrer: None,
            transition_Type: None,
            frame_id: None,
            referrer_policy: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if let Ok(val) =
                tab.evaluate("!!document.querySelector('embed, object, iframe')", false)
            {
                if val.value.as_ref().and_then(|v| v.as_bool()) == Some(true) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        std::thread::sleep(Duration::from_millis(1500));
    } else {
        tab.navigate_to(&url)
            .and_then(|t| t.wait_until_navigated())
            .map_err(|e| Error::Export(format!("navigate to {url}: {e}")))?;
    }

    eprintln!(
        "● recording the browser — navigate freely, then CLOSE THE WINDOW to finish the scene"
    );
    let delay = Duration::from_millis((1000 / VIEW_FPS.max(1)) as u64);
    let max_frames = VIEW_FPS as usize * 300; // 5-minute safety cap
    let mut n = 0usize;
    loop {
        // A failed screenshot means the tab/window was closed — that's the cue to
        // stop recording. No clip: clip x/y are document coordinates, so once the
        // user scrolls, a (0,0) clip would capture unrasterized background.
        let Ok(png) = tab.capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
        else {
            break;
        };
        let path = out_dir.join(format!("{:04}.png", n + 1));
        if std::fs::write(&path, &png).is_err() {
            break;
        }
        n += 1;
        if n >= max_frames {
            break;
        }
        std::thread::sleep(delay);
    }
    Ok(n)
}

/// Load a `--view` scene's pre-recorded PNG frames (`NNNN.png`, sorted) as a
/// directory-backed frame source sized to `tw`×`th`. Frames are decoded on
/// demand; only one frame is held decoded at a time.
fn load_frames(dir: &Path, tw: usize, th: usize) -> Result<DirScene> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| Error::io(dir, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("png"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(Error::Export(format!(
            "no recorded frames in {} (was the --view window closed before anything rendered?)",
            dir.display()
        )));
    }

    let count = files.len();
    let progresses: Vec<f64> = (0..count)
        .map(|i| {
            if count > 1 {
                i as f64 / (count - 1) as f64
            } else {
                0.0
            }
        })
        .collect();
    Ok(DirScene::new(tw, th, files, progresses))
}

/// Emulate `prefers-color-scheme` (`light`/`dark`) so theme-aware pages render the
/// chosen theme. A no-op when `theme` is `None`; failures are non-fatal.
fn emulate_theme(tab: &Arc<Tab>, theme: Option<&str>) {
    if let Some(scheme) = theme {
        let _ = tab.call_method(SetEmulatedMedia {
            media: None,
            features: Some(vec![MediaFeature {
                name: "prefers-color-scheme".to_string(),
                value: scheme.to_string(),
            }]),
        });
    }
}

/// Extract the local filesystem path from a `file:///path` or
/// `http://127.0.0.1:PORT/path` URL (the local server's root is `/`, so its
/// request path *is* the filesystem path).
fn local_file_path(url: &str) -> Result<std::path::PathBuf> {
    let local_path = if let Some(rest) = url.strip_prefix("file://") {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix("http://127.0.0.1:") {
        match rest.find('/') {
            Some(slash) => rest[slash..].to_string(),
            None => {
                return Err(Error::Export(format!("can't extract path from URL: {url}")));
            }
        }
    } else {
        return Err(Error::Export(format!("can't extract path from URL: {url}")));
    };
    let path = std::path::PathBuf::from(&local_path);
    if !path.exists() {
        return Err(Error::Export(format!("file not found: {local_path}")));
    }
    Ok(path)
}

fn shot(tab: &Arc<Tab>, w: usize, h: usize) -> Result<Vec<u8>> {
    // No clip: a clip's x/y are DOCUMENT coordinates, so after a scroll the
    // clipped region is off-screen and Chrome returns unrasterized background.
    // The viewport is already forced to w×h; png_to_rgba crops/pads any excess.
    let png = tab
        .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
        .map_err(|e| Error::Export(format!("screenshot: {e}")))?;
    png_to_rgba(&png, w, h)
}

/// Decode a PNG screenshot into a `tw`×`th` RGBA buffer (cropping/padding to fit).
fn png_to_rgba(bytes: &[u8], tw: usize, th: usize) -> Result<Vec<u8>> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::Export(format!("png: {e}")))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| Error::Export(format!("png: {e}")))?;

    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        other => {
            return Err(Error::Export(format!(
                "unsupported screenshot format: {other:?}"
            )))
        }
    };
    let (sw, sh) = (info.width as usize, info.height as usize);

    let mut out = vec![0u8; tw * th * 4];
    for y in 0..th.min(sh) {
        for x in 0..tw.min(sw) {
            let s = (y * sw + x) * channels;
            let d = (y * tw + x) * 4;
            out[d] = buf[s];
            out[d + 1] = buf[s + 1];
            out[d + 2] = buf[s + 2];
            out[d + 3] = 255;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{capture_budget, web_pane_offsets, Scene};
    use crate::model::{ScrollDirection, Velocity};

    #[test]
    fn frame_at_picks_the_latest_keyframe() {
        let scene = Scene {
            width: 1,
            height: 1,
            keyframes: vec![(0.0, vec![1]), (0.5, vec![2]), (0.9, vec![3])],
        };
        assert_eq!(scene.frame_at(0.0), &[1]);
        assert_eq!(scene.frame_at(0.49), &[1]);
        assert_eq!(scene.frame_at(0.5), &[2]);
        assert_eq!(scene.frame_at(0.95), &[3]);
        assert_eq!(scene.frame_at(2.0), &[3]);
    }

    #[test]
    fn frame_at_single_keyframe() {
        let scene = Scene {
            width: 1,
            height: 1,
            keyframes: vec![(0.0, vec![42])],
        };
        assert_eq!(scene.frame_at(0.0), &[42]);
        assert_eq!(scene.frame_at(1.0), &[42]);
    }

    #[test]
    fn frame_at_boundary_between_keyframes() {
        let scene = Scene {
            width: 1,
            height: 1,
            keyframes: vec![(0.0, vec![1]), (0.5, vec![2])],
        };
        assert_eq!(scene.frame_at(0.5), &[2]);
        assert_eq!(scene.frame_at(0.499), &[1]);
    }

    #[test]
    fn local_file_path_extracts_from_file_url() {
        // Non-existent file should error (file doesn't exist)
        let result = super::local_file_path("file:///tmp/nonexistent_test_file_12345.pdf");
        assert!(result.is_err());
    }

    #[test]
    fn local_file_path_extracts_from_localhost_url() {
        // Non-existent file should error
        let result = super::local_file_path("http://127.0.0.1:8080/nonexistent.pdf");
        assert!(result.is_err());
    }

    #[test]
    fn local_file_path_rejects_non_local_url() {
        let result = super::local_file_path("https://example.com/page.html");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("can't extract path"));
    }

    #[test]
    fn local_file_path_rejects_localhost_without_path() {
        let result = super::local_file_path("http://127.0.0.1:8080");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("can't extract path"));
    }

    #[test]
    fn local_file_path_rejects_file_url_without_path() {
        let result = super::local_file_path("file://");
        assert!(result.is_err());
    }

    #[test]
    fn local_file_path_rejects_ftp_url() {
        let result = super::local_file_path("ftp://example.com/file.pdf");
        assert!(result.is_err());
    }

    #[test]
    fn from_keyframes_builds_scene() {
        let scene = Scene {
            width: 100,
            height: 200,
            keyframes: vec![(0.0, vec![1, 2, 3])],
        };
        assert_eq!(scene.width, 100);
        assert_eq!(scene.height, 200);
        assert_eq!(scene.frame_at(0.5), &[1, 2, 3]);
    }

    #[test]
    fn from_keyframes_empty_keyframes() {
        let scene = Scene {
            width: 10,
            height: 10,
            keyframes: vec![],
        };
        assert_eq!(scene.width, 10);
        assert_eq!(scene.height, 10);
    }

    #[test]
    fn png_to_rgba_rejects_invalid_png() {
        let result = super::png_to_rgba(&[0, 1, 2, 3], 10, 10);
        assert!(result.is_err());
    }

    #[test]
    fn png_to_rgba_converts_valid_png() {
        // Create a minimal 2x2 RGBA PNG using png crate properly
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut encoder = png::Encoder::new(&mut buf, 2, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            let pixels: Vec<u8> = vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
            ];
            writer.write_image_data(&pixels).unwrap();
            writer.finish().unwrap();
        }
        let png_bytes = buf.into_inner();

        let rgba = super::png_to_rgba(&png_bytes, 2, 2).unwrap();
        assert_eq!(rgba.len(), 2 * 2 * 4);
        // Top-left pixel should be red
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
        // Top-right pixel should be green
        assert_eq!(&rgba[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn png_to_rgba_crops_to_target_size() {
        // Create a 4x4 RGB PNG
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut encoder = png::Encoder::new(&mut buf, 4, 4);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            let pixels: Vec<u8> = (0..4 * 4 * 3).map(|i| i as u8).collect();
            writer.write_image_data(&pixels).unwrap();
            writer.finish().unwrap();
        }
        let png_bytes = buf.into_inner();

        // Target is 2x2 — should crop to top-left
        let rgba = super::png_to_rgba(&png_bytes, 2, 2).unwrap();
        assert_eq!(rgba.len(), 2 * 2 * 4);
        // All alpha should be 255
        for px in rgba.as_chunks::<4>().0 {
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn png_to_rgba_pads_when_target_larger() {
        // Create a 1x1 RGB PNG
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut encoder = png::Encoder::new(&mut buf, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 128, 64]).unwrap();
            writer.finish().unwrap();
        }
        let png_bytes = buf.into_inner();

        // Target is 3x3 — should have the pixel at (0,0) and zeros elsewhere
        let rgba = super::png_to_rgba(&png_bytes, 3, 3).unwrap();
        assert_eq!(rgba.len(), 3 * 3 * 4);
        assert_eq!(&rgba[0..4], &[255, 128, 64, 255]);
        // Second pixel should be black
        assert_eq!(&rgba[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn png_to_rgba_rejects_unsupported_color_type() {
        // Create a 1x1 grayscale PNG
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut encoder = png::Encoder::new(&mut buf, 1, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[128]).unwrap();
            writer.finish().unwrap();
        }
        let png_bytes = buf.into_inner();

        let result = super::png_to_rgba(&png_bytes, 1, 1);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unsupported screenshot format"));
    }

    #[test]
    fn view_fps_constant() {
        assert_eq!(super::VIEW_FPS, 8);
    }

    #[test]
    fn scene_from_keyframes_preserves_dimensions() {
        let scene = Scene {
            width: 640,
            height: 480,
            keyframes: vec![],
        };
        assert_eq!(scene.width, 640);
        assert_eq!(scene.height, 480);
    }

    #[test]
    fn frame_at_negative_progress_returns_first() {
        let scene = Scene {
            width: 1,
            height: 1,
            keyframes: vec![(0.0, vec![10]), (0.5, vec![20])],
        };
        assert_eq!(scene.frame_at(-1.0), &[10]);
    }

    fn make_test_png(r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut encoder = png::Encoder::new(&mut buf, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[r, g, b]).unwrap();
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    struct TmpDir(std::path::PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_dir_scene(n: usize) -> (TmpDir, super::DirScene) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "demostage_test_dir_scene_{}_{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..n {
            let png = make_test_png(i as u8, 0, 0);
            let path = dir.join(format!("{:04}.png", i + 1));
            std::fs::write(&path, &png).unwrap();
        }
        let scene = super::load_frames(&dir, 1, 1).unwrap();
        (TmpDir(dir), scene)
    }

    #[test]
    fn dir_scene_frame_at_several_progress_values() {
        let (_dir, mut scene) = make_dir_scene(10);
        // progress 0.0 → first frame (index 0, red=0)
        let f = scene.frame_at(0.0);
        assert_eq!(f[0], 0);
        // progress at exact boundary (index 5, progress = 5/9 ≈ 0.555)
        let f = scene.frame_at(5.0 / 9.0);
        assert_eq!(f[0], 5);
        // progress beyond 1.0 → last frame (index 9, red=9)
        let f = scene.frame_at(2.0);
        assert_eq!(f[0], 9);
        // progress just before a boundary
        let f = scene.frame_at(0.5);
        // 0.5 < 5/9 ≈ 0.555, so index 4 (progress = 4/9 ≈ 0.444)
        assert_eq!(f[0], 4);
    }

    #[test]
    fn dir_scene_decode_count_is_bounded() {
        let (_dir, mut scene) = make_dir_scene(100);
        // Walk progress monotonically from 0 to 1 in 50 steps.
        // We land on at most 50 unique frame indices, so decode count <= 50.
        // Crucially, it is NOT 100 (we don't decode every frame).
        for step in 0..50 {
            let progress = step as f64 / 49.0;
            let _ = scene.frame_at(progress);
        }
        let decodes = scene.decode_count();
        assert!(
            decodes <= 50,
            "expected at most 50 decodes for 50 steps, got {decodes}"
        );
        assert!(decodes > 0, "expected at least one decode");
    }

    #[test]
    fn dir_scene_empty_directory_errors() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "demostage_test_empty_{}_{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let result = super::load_frames(&dir, 100, 100);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no recorded frames"));
        assert!(err.contains("--view"));
    }

    #[test]
    fn dir_scene_single_frame() {
        let (_dir, mut scene) = make_dir_scene(1);
        // Single frame: progress is always 0.0
        let f = scene.frame_at(0.0);
        assert_eq!(f[0], 0);
        let f = scene.frame_at(1.0);
        assert_eq!(f[0], 0);
    }

    /// A browser frame costs a CDP screenshot (~291 ms measured), a PDF frame a
    /// memcpy. The budget is what keeps the expensive path from paying the cheap
    /// path's price: a 10 s pane drops from ~300 captures to ~40.
    #[test]
    fn a_web_pane_captures_at_a_bounded_rate() {
        let ten_seconds_at_30fps = 300;
        let budget = capture_budget(ten_seconds_at_30fps, 30.0);
        assert_eq!(budget, 40);
        assert!(budget < ten_seconds_at_30fps);
    }

    #[test]
    fn the_budget_never_exceeds_the_frames_available() {
        // 2 frames at 1 fps is 2 s of pane, which the rate would price at 8
        // captures — but there are only 2 frames to capture.
        assert_eq!(capture_budget(2, 1.0), 2);
        assert_eq!(capture_budget(1, 30.0), 1);
        // A tenth of a second is worth one frame, never zero.
        assert_eq!(capture_budget(3, 30.0), 1);
    }

    /// A browser pane that no `scroll` step targets must stay a still, however
    /// tall the page is. Regression: the flag reached `capture`, was forwarded to
    /// the PDF branch, and the Chrome branch ignored it — so the ghscaff demo,
    /// whose score has no scroll step at all, went from 1 static frame to 141
    /// scrolling ones and spent 41 s capturing them.
    #[test]
    fn a_pane_with_no_scroll_step_is_one_still_frame() {
        let offsets = web_pane_offsets(None, 141, None);
        assert_eq!(offsets, vec![0], "a pane nobody scrolled must not scroll");
    }

    #[test]
    fn a_pane_with_a_scroll_step_still_ramps() {
        let offsets = web_pane_offsets(
            Some((20_000, 1080)),
            141,
            Some((ScrollDirection::Down, Velocity::Constant)),
        );
        assert_eq!(offsets.len(), 141);
        assert_eq!(offsets[0], 0);
        assert_eq!(*offsets.last().unwrap(), 20_000 - 1080);
    }

    #[test]
    fn scroll_offsets_first_zero_last_max_strictly_increasing() {
        let offsets =
            super::scroll_offsets(2000, 500, 10, ScrollDirection::Down, Velocity::Constant);
        assert_eq!(offsets.len(), 10);
        assert_eq!(*offsets.first().unwrap(), 0);
        assert_eq!(*offsets.last().unwrap(), 1500); // 2000 - 500
        for w in offsets.windows(2) {
            assert!(w[1] > w[0], "offsets must be strictly increasing");
        }
    }

    #[test]
    fn scroll_offsets_page_not_scrollable_yields_single_frame() {
        let offsets =
            super::scroll_offsets(500, 500, 300, ScrollDirection::Down, Velocity::Constant);
        assert_eq!(offsets, vec![0]);
        let offsets =
            super::scroll_offsets(300, 500, 300, ScrollDirection::Down, Velocity::Constant);
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn scroll_offsets_single_frame_request() {
        let offsets =
            super::scroll_offsets(2000, 500, 1, ScrollDirection::Down, Velocity::Constant);
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn scroll_offsets_two_frames() {
        let offsets =
            super::scroll_offsets(2000, 500, 2, ScrollDirection::Down, Velocity::Constant);
        assert_eq!(offsets, vec![0, 1500]);
    }

    #[test]
    fn temp_dir_guard_removes_directory_on_drop() {
        let dir = std::env::temp_dir().join(format!(
            "demostage_test_guard_{}_{}",
            std::process::id(),
            999
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sentinel = dir.join("sentinel.txt");
        std::fs::write(&sentinel, b"x").unwrap();
        assert!(dir.exists());
        {
            let _guard = super::TempDirGuard(Some(dir.clone()));
            assert!(dir.exists());
        }
        assert!(!dir.exists());
    }

    #[test]
    fn temp_dir_guard_none_is_noop() {
        let _guard = super::TempDirGuard(None);
    }

    #[test]
    fn capture_frames_to_dir_success_writes_all_frames() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "demostage_test_capture_ok_{}_{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = super::TempDirGuard(Some(dir.clone()));

        let offsets = vec![0, 500, 1000, 1500];
        let files =
            super::capture_frames_to_dir(&offsets, &dir, |i| Ok(make_test_png(i as u8, 0, 0)))
                .unwrap();

        assert_eq!(files.len(), 4);
        for (i, path) in files.iter().enumerate() {
            assert!(path.exists(), "frame {i} should exist");
            assert!(path.starts_with(&dir));
            assert_eq!(
                path.file_name().unwrap().to_str().unwrap(),
                format!("{:04}.png", i + 1)
            );
        }
    }

    #[test]
    fn capture_frames_to_dir_failure_returns_error_and_cleanup_removes_dir() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "demostage_test_capture_fail_{}_{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let guard = super::TempDirGuard(Some(dir.clone()));

        let offsets = vec![0, 500, 1000];
        let result = super::capture_frames_to_dir(&offsets, &dir, |i| {
            if i == 1 {
                Err(crate::error::Error::Export(format!(
                    "simulated screenshot failure at frame {i}"
                )))
            } else {
                Ok(make_test_png(i as u8, 0, 0))
            }
        });

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("screenshot frame 1"));
        assert!(err.contains("simulated screenshot failure"));

        drop(guard);
        assert!(
            !dir.exists(),
            "temp directory should be removed after failure path"
        );
    }

    // --- mutant-killing tests: pdf gate, chrome progress, native render ---

    #[test]
    fn is_pdf_url_matches_case_insensitively() {
        assert!(super::is_pdf_url("file:///tmp/doc.pdf"));
        assert!(super::is_pdf_url("https://x.com/DOC.PDF"));
        assert!(super::is_pdf_url("https://x.com/dOc.PdF"));
        assert!(!super::is_pdf_url("https://x.com/page.html"));
        assert!(!super::is_pdf_url("https://x.com/pdf/viewer"));
        assert!(!super::is_pdf_url(""));
    }

    #[test]
    fn chrome_pdf_progress_spans_half_to_one() {
        // scroll_keyframes = 2 → first scroll shot 0.75, last exactly 1.0.
        assert_eq!(super::chrome_pdf_progress(0, 2), 0.75);
        assert_eq!(super::chrome_pdf_progress(1, 2), 1.0);
        // scroll_keyframes = 1 → single shot exactly 1.0.
        assert_eq!(super::chrome_pdf_progress(0, 1), 1.0);
        // Strictly increasing within a run.
        let run: Vec<f64> = (0..4).map(|i| super::chrome_pdf_progress(i, 4)).collect();
        assert_eq!(run, vec![0.625, 0.75, 0.875, 1.0]);
    }

    /// One blank page with computed xref offsets, so the structure is valid
    /// by construction.
    fn minimal_pdf_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");
        let objs = [
            "1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n",
            "2 0 obj\n<</Type/Pages/Kids[3 0 R]/Count 1>>\nendobj\n",
            "3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>\nendobj\n",
        ];
        let mut offsets = Vec::new();
        for o in objs {
            offsets.push(out.len());
            out.extend_from_slice(o.as_bytes());
        }
        let xref_at = out.len();
        out.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<</Size 4/Root 1 0 R>>\nstartxref\n{xref_at}\n%%EOF\n").as_bytes(),
        );
        out
    }

    #[test]
    fn native_pdf_scene_rejects_non_pdf_without_rendering() {
        assert!(
            super::native_pdf_scene("https://x.com/page.html", 800, 600, 2, 15.0, None, 1.0)
                .is_none()
        );
    }

    #[test]
    fn native_pdf_scene_renders_a_real_pdf_file() {
        let dir = std::env::temp_dir().join(format!(
            "demo-test-native-pdf-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("doc.pdf");
        std::fs::write(&pdf, minimal_pdf_bytes()).unwrap();
        let url = format!("file://{}", pdf.display());
        let got = super::native_pdf_scene(&url, 800, 600, 2, 15.0, None, 1.0);
        assert!(got.is_some(), "a valid local PDF must render natively");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
