use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::RenderArgs;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RenderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub visual_fps: u32,
    pub bars: usize,
    pub analysis_sample_rate: u32,
    #[serde(default = "default_fft_size")]
    pub fft_size: usize,
    pub encoder: String,
    pub quality: u8,
    pub preset: Option<String>,
    pub audio_bitrate: String,
    pub show_title: bool,
}

impl TryFrom<&RenderArgs> for RenderConfig {
    type Error = anyhow::Error;

    fn try_from(cli: &RenderArgs) -> Result<Self> {
        if cli.width < 640 || cli.height < 360 {
            bail!("video dimensions must be at least 640x360");
        }
        if !cli.width.is_multiple_of(2) || !cli.height.is_multiple_of(2) {
            bail!("video dimensions must be even for yuv420p output");
        }
        if cli.fps == 0 || cli.visual_fps == 0 || cli.visual_fps > cli.fps {
            bail!("visual FPS must be between 1 and the output FPS");
        }
        if !(24..=256).contains(&cli.bars) || !cli.bars.is_multiple_of(2) {
            bail!("bars must be an even number between 24 and 256");
        }
        if !(4_000..=96_000).contains(&cli.analysis_sample_rate) {
            bail!("analysis sample rate must be between 4000 and 96000 Hz");
        }
        if !(256..=8_192).contains(&cli.fft_size) || !cli.fft_size.is_power_of_two() {
            bail!("FFT size must be a power of two between 256 and 8192");
        }
        if cli.quality > 51 {
            bail!("quality must be between 0 and 51");
        }
        if cli.jobs == 0 {
            bail!("jobs must be positive");
        }
        if cli.limit == Some(0) {
            bail!("limit must be positive");
        }
        if cli
            .max_duration
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            bail!("max duration must be positive");
        }

        Ok(Self {
            width: cli.width,
            height: cli.height,
            fps: cli.fps,
            visual_fps: cli.visual_fps,
            bars: cli.bars,
            analysis_sample_rate: cli.analysis_sample_rate,
            fft_size: cli.fft_size,
            encoder: cli.encoder.ffmpeg_name().to_owned(),
            quality: cli.quality,
            preset: cli.preset.clone(),
            audio_bitrate: cli.audio_bitrate.clone(),
            show_title: !cli.no_title,
        })
    }
}

const fn default_fft_size() -> usize {
    2_048
}
