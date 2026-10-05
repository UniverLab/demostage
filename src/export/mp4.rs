//! MP4 target (H.264 via ffmpeg, auto-provisioned). `encode` is source-agnostic,
//! so both the single-terminal fast path and the multi-scene stage feed it.

use std::io::{Read, Write};
use std::path::Path;
use std::thread;

use ffmpeg_sidecar::command::FfmpegCommand;

use super::run::Recording;
use super::{provision, raster};
use crate::error::{Error, Result};
use crate::model::Score;

/// Where an encoder's frames go: the encoder calls it with each `w`×`h` RGBA
/// frame as it is produced.
pub type FrameSink<'a> = &'a mut dyn FnMut(&[u8]);

/// The frame source an encoder drives: it is handed a [`FrameSink`] and pushes
/// every frame of the export into it.
pub type FrameSource<'a> = &'a mut dyn FnMut(FrameSink<'_>) -> Result<()>;

/// Encode an MP4 at `path` from the frames `render` pushes into the sink it is
/// handed (each frame `w`×`h` RGBA). ffmpeg is provisioned on first use.
///
/// The frame source is a [`FrameSource`] trait object (not a generic closure) so
/// this doubles as a value: [`crate::export::render`] takes the encoder as a
/// parameter, which lets the render path be tested without a real ffmpeg.
pub fn encode(path: &Path, w: usize, h: usize, fps: u32, render: FrameSource<'_>) -> Result<()> {
    provision::ensure_ffmpeg()?;

    let size = format!("{w}x{h}");
    let fps = fps.to_string();
    let out = path.to_string_lossy().to_string();

    let mut child = FfmpegCommand::new()
        .args([
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            &size,
            "-framerate",
            &fps,
            "-i",
            "-",
        ])
        .args([
            "-c:v",
            "libx264",
            // `slow` spends encode time for a noticeably smaller file at the
            // same CRF quality target — demos are short, so it's cheap.
            "-preset",
            "slow",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            "-y",
            &out,
        ])
        .spawn()
        .map_err(|e| Error::Export(format!("spawn ffmpeg: {e}")))?;

    let mut stdin = child
        .take_stdin()
        .ok_or_else(|| Error::Export("ffmpeg stdin unavailable".to_string()))?;

    // Drain ffmpeg's stderr on a side thread so a failing encode (e.g. a frame
    // size mismatch) reports *why* it died instead of a bare "broken pipe" —
    // and so a full stderr pipe can't deadlock the frame writes.
    let stderr_thread = child.take_stderr().map(|mut err| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        })
    });

    let mut write_err: Option<Error> = None;
    render(&mut |frame: &[u8]| {
        if write_err.is_some() {
            return;
        }
        if let Err(e) = stdin.write_all(frame) {
            write_err = Some(Error::Export(format!("piping frame to ffmpeg: {e}")));
        }
    })?;
    drop(stdin);

    let status = child
        .wait()
        .map_err(|e| Error::Export(format!("ffmpeg: {e}")))?;
    let stderr = stderr_thread
        .and_then(|t| t.join().ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let tail = ffmpeg_tail(&stderr);
    if let Some(e) = write_err {
        return Err(Error::Export(format!("{e}{tail}")));
    }
    if !status.success() {
        return Err(Error::Export(format!(
            "ffmpeg failed to encode the mp4{tail}"
        )));
    }
    Ok(())
}

/// Condense ffmpeg's stderr (banner + build config + progress) into the last few
/// non-empty lines — where the actual error lives — for a readable message.
fn ffmpeg_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let tail = lines[lines.len().saturating_sub(4)..].join("\n");
    format!("\nffmpeg: {tail}")
}

/// Single-terminal fast path: encode an MP4 straight from a recording.
pub fn write_mp4(rec: &Recording, score: &Score, path: &Path) -> Result<()> {
    let plan = raster::plan(rec, score);
    encode(path, plan.width, plan.height, plan.fps, &mut |emit| {
        raster::render_frames(rec, score, |f| emit(f)).map(|_| ())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_tail_empty_string() {
        assert_eq!(ffmpeg_tail(""), "");
    }

    #[test]
    fn ffmpeg_tail_only_whitespace() {
        assert_eq!(ffmpeg_tail("  \n  \n  "), "");
    }

    #[test]
    fn ffmpeg_tail_single_line() {
        let result = ffmpeg_tail("error: something failed");
        assert_eq!(result, "\nffmpeg: error: something failed");
    }

    #[test]
    fn ffmpeg_tail_few_lines() {
        let input = "line1\nline2\nline3";
        let result = ffmpeg_tail(input);
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(result.starts_with("\nffmpeg:"));
    }

    #[test]
    fn ffmpeg_tail_trims_trailing_whitespace() {
        let input = "line1   \nline2  \n";
        let result = ffmpeg_tail(input);
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(!result.contains("   "));
    }

    #[test]
    fn ffmpeg_tail_keeps_last_four_lines() {
        let input = "a\nb\nc\nd\ne\nf";
        let result = ffmpeg_tail(input);
        assert!(!result.contains("\na\n"));
        assert!(!result.contains("\nb\n"));
        assert!(!result.contains("\nc\n"));
        assert!(result.contains("d"));
        assert!(result.contains("e"));
        assert!(result.contains("f"));
    }

    #[test]
    fn ffmpeg_tail_skips_empty_lines() {
        let input = "line1\n\n\nline2\n";
        let result = ffmpeg_tail(input);
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
    }
}
