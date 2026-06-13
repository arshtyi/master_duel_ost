use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

use anyhow::{Context, Result, bail};

use crate::{audio::AudioAnalysis, config::RenderConfig, discovery::Track, renderer::Renderer};

pub struct RenderStats {
    pub duration_seconds: f64,
    pub elapsed_seconds: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct RenderProgress {
    pub media_seconds: f64,
    pub duration_seconds: f64,
}

pub fn resolve_encoder(requested: &str) -> Result<String> {
    if requested != "auto" {
        if probe_encoder(requested) {
            return Ok(requested.to_owned());
        }
        bail!("ffmpeg encoder {requested} is unavailable on this system");
    }

    for encoder in [
        "h264_nvenc",
        "h264_qsv",
        "h264_vaapi",
        "h264_amf",
        "libx264",
        "libopenh264",
    ] {
        if probe_encoder(encoder) {
            return Ok(encoder.to_owned());
        }
    }
    bail!("no supported H.264 encoder is available in ffmpeg")
}

fn probe_encoder(encoder: &str) -> bool {
    let mut command = Command::new("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error"]);
    if encoder == "h264_vaapi" {
        let Some(device) = vaapi_device() else {
            return false;
        };
        command.arg("-vaapi_device").arg(device);
    }
    command.args([
        "-f",
        "lavfi",
        "-i",
        "color=black:s=64x64:d=0.04",
        "-frames:v",
        "1",
    ]);
    if encoder == "h264_vaapi" {
        command.args(["-vf", "format=nv12,hwupload"]);
    }
    command
        .args(["-c:v", encoder, "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn render_track<F>(
    track: &Track,
    config: &RenderConfig,
    max_duration: Option<f64>,
    encoder_threads: usize,
    mut report_progress: F,
) -> Result<RenderStats>
where
    F: FnMut(RenderProgress),
{
    let started = Instant::now();
    let analysis = AudioAnalysis::decode(
        &track.source,
        config.analysis_sample_rate,
        config.visual_fps,
        config.bars,
        config.fft_size,
        max_duration,
    )?;
    let mut renderer = Renderer::new(config, &track.title)?;
    let duration = analysis.duration;
    if duration <= 0.0 {
        bail!("audio has no positive duration: {}", track.source.display());
    }
    report_progress(RenderProgress {
        media_seconds: 0.0,
        duration_seconds: duration,
    });

    let parent = track.output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temporary = track.output.with_extension("part.mp4");
    if temporary.exists() {
        fs::remove_file(&temporary)
            .with_context(|| format!("failed to replace {}", temporary.display()))?;
    }

    let mut command = build_encode_command(track, &temporary, config, duration, encoder_threads);
    let mut child = command
        .spawn()
        .with_context(|| "failed to launch ffmpeg video encoder")?;
    let mut input = child
        .stdin
        .take()
        .context("ffmpeg stdin was not available")?;
    let frame_count = (duration * config.visual_fps as f64).ceil() as usize;
    let mut last_report = Instant::now();
    let write_result = (|| -> Result<()> {
        for frame_index in 0..frame_count {
            let time = frame_index as f32 / config.visual_fps as f32;
            let spectrum = analysis.spectrum_at(time);
            let energy = analysis.energy_at(time);
            let frame = renderer.frame(spectrum, energy, time / duration as f32);
            input
                .write_all(frame)
                .with_context(|| "ffmpeg stopped accepting video frames")?;
            if last_report.elapsed().as_millis() >= 400 || frame_index + 1 == frame_count {
                report_progress(RenderProgress {
                    media_seconds: ((frame_index + 1) as f64 / config.visual_fps as f64)
                        .min(duration),
                    duration_seconds: duration,
                });
                last_report = Instant::now();
            }
        }
        Ok(())
    })();
    drop(input);
    let output = child
        .wait_with_output()
        .with_context(|| "failed while waiting for ffmpeg")?;
    if !output.status.success() {
        let _ = fs::remove_file(&temporary);
        bail!(
            "ffmpeg encoding failed for {}: {}",
            track.source.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, &track.output).with_context(|| {
        format!(
            "failed to move {} to {}",
            temporary.display(),
            track.output.display()
        )
    })?;
    Ok(RenderStats {
        duration_seconds: duration,
        elapsed_seconds: started.elapsed().as_secs_f64(),
    })
}

fn build_encode_command(
    track: &Track,
    output: &Path,
    config: &RenderConfig,
    duration: f64,
    encoder_threads: usize,
) -> Command {
    let mut command = Command::new("ffmpeg");
    if config.encoder == "h264_vaapi"
        && let Some(device) = vaapi_device()
    {
        command.arg("-vaapi_device").arg(device);
    }
    let video_filter = if config.encoder == "h264_vaapi" {
        format!("fps={},format=nv12,hwupload", config.fps)
    } else {
        format!("fps={},format=yuv420p", config.fps)
    };
    command
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "rawvideo"])
        .args(["-pixel_format", "rgba", "-video_size"])
        .arg(format!("{}x{}", config.width, config.height))
        .args([
            "-framerate",
            &config.visual_fps.to_string(),
            "-i",
            "pipe:0",
            "-i",
        ])
        .arg(&track.source)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-t"])
        .arg(format!("{duration:.6}"))
        .args(["-vf", &video_filter])
        .args(["-c:v", &config.encoder]);
    add_encoder_options(&mut command, config, encoder_threads);
    command
        .args(["-c:a", "aac", "-b:a", &config.audio_bitrate])
        .args(["-movflags", "+faststart", "-shortest"])
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn add_encoder_options(command: &mut Command, config: &RenderConfig, encoder_threads: usize) {
    match config.encoder.as_str() {
        "h264_nvenc" => {
            command.args([
                "-preset",
                config.preset.as_deref().unwrap_or("p4"),
                "-rc",
                "vbr",
                "-cq",
                &config.quality.to_string(),
            ]);
        }
        "h264_qsv" => {
            command.args([
                "-preset",
                config.preset.as_deref().unwrap_or("medium"),
                "-global_quality",
                &config.quality.to_string(),
            ]);
        }
        "h264_amf" => {
            command.args([
                "-quality",
                config.preset.as_deref().unwrap_or("speed"),
                "-q:v",
                &config.quality.to_string(),
            ]);
        }
        "h264_vaapi" => {
            command.args(["-qp", &config.quality.to_string()]);
        }
        "libx264" => {
            command.args([
                "-preset",
                config.preset.as_deref().unwrap_or("veryfast"),
                "-crf",
                &config.quality.to_string(),
                "-threads",
                &encoder_threads.to_string(),
            ]);
        }
        "libopenh264" => {
            let bitrate = openh264_bitrate(config.width, config.height, config.fps, config.quality);
            let gop = (config.fps / 5).max(1);
            command.args([
                "-b:v",
                &bitrate.to_string(),
                "-profile:v",
                "high",
                "-coder",
                "cabac",
                "-rc_mode",
                "quality",
                "-g",
                &gop.to_string(),
                "-threads",
                &encoder_threads.to_string(),
            ]);
        }
        _ => {}
    }
}

fn openh264_bitrate(width: u32, height: u32, fps: u32, quality: u8) -> u64 {
    const REFERENCE_PIXEL_RATE: u64 = 2_560 * 1_440 * 60;
    const REFERENCE_BITRATE: f64 = 18_000_000.0;
    const MINIMUM_BITRATE: f64 = 2_000_000.0;
    const MAXIMUM_BITRATE: f64 = 80_000_000.0;

    let pixel_rate = u64::from(width) * u64::from(height) * u64::from(fps);
    let resolution_scale = pixel_rate as f64 / REFERENCE_PIXEL_RATE as f64;
    let quality_scale = 2.0_f64.powf((18.0 - f64::from(quality)) / 6.0);
    (REFERENCE_BITRATE * resolution_scale * quality_scale)
        .round()
        .clamp(MINIMUM_BITRATE, MAXIMUM_BITRATE) as u64
}

fn vaapi_device() -> Option<PathBuf> {
    if let Some(device) = std::env::var_os("VAAPI_DEVICE") {
        return Some(PathBuf::from(device));
    }
    let mut devices: Vec<PathBuf> = fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    devices.sort();
    devices.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openh264_quality_scales_reference_bitrate() {
        let default = openh264_bitrate(2_560, 1_440, 60, 18);

        assert_eq!(default, 18_000_000);
        assert!(openh264_bitrate(2_560, 1_440, 60, 12) > default);
        assert!(openh264_bitrate(2_560, 1_440, 60, 24) < default);
    }
}
