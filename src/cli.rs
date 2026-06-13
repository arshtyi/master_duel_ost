use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum EncoderChoice {
    Auto,
    H264Nvenc,
    H264Qsv,
    H264Vaapi,
    H264Amf,
    Libx264,
    Libopenh264,
}

impl EncoderChoice {
    pub const fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::H264Nvenc => "h264_nvenc",
            Self::H264Qsv => "h264_qsv",
            Self::H264Vaapi => "h264_vaapi",
            Self::H264Amf => "h264_amf",
            Self::Libx264 => "libx264",
            Self::Libopenh264 => "libopenh264",
        }
    }
}

#[derive(Debug, Parser)]
#[command(version, about = "Batch-render circular OST visualizers")]
pub struct Cli {
    /// Audio file or directory to scan recursively.
    #[arg(default_value = "AudioClip")]
    pub input: PathBuf,

    /// Directory for generated MP4 files.
    #[arg(short, long, default_value = "outputs")]
    pub output_dir: PathBuf,

    /// Persistent incremental-render summary.
    #[arg(long, default_value = "render-summary.json")]
    pub summary: PathBuf,

    /// Output width in pixels.
    #[arg(long, default_value_t = 2560)]
    pub width: u32,

    /// Output height in pixels.
    #[arg(long, default_value_t = 1440)]
    pub height: u32,

    /// Encoded output frame rate.
    #[arg(long, default_value_t = 60)]
    pub fps: u32,

    /// Unique visual frames rendered per second; FFmpeg fills the output frame rate.
    #[arg(long, default_value_t = 24)]
    pub visual_fps: u32,

    /// Even number of radial spectrum bars.
    #[arg(long, default_value_t = 72)]
    pub bars: usize,

    /// Mono sample rate used only for spectrum analysis.
    #[arg(long, default_value_t = 12_000)]
    pub analysis_sample_rate: u32,

    /// Power-of-two FFT window size used by the spectrum analyzer.
    #[arg(long, default_value_t = 2_048)]
    pub fft_size: usize,

    /// H.264 encoder; auto prefers usable hardware encoders.
    #[arg(long, value_enum, default_value_t = EncoderChoice::Auto)]
    pub encoder: EncoderChoice,

    /// Video quality from 0 (best/largest) to 51 (lowest/smallest).
    #[arg(long, default_value_t = 18)]
    pub quality: u8,

    /// Encoder-specific speed preset; omitted values use tuned defaults.
    #[arg(long)]
    pub preset: Option<String>,

    /// AAC bitrate in FFmpeg notation, for example 320k.
    #[arg(long, default_value = "320k")]
    pub audio_bitrate: String,

    /// Number of videos rendered concurrently.
    #[arg(short, long, default_value_t = 2)]
    pub jobs: usize,

    /// Render even when the summary says an output is current.
    #[arg(long)]
    pub overwrite: bool,

    /// Hide the header and track title.
    #[arg(long)]
    pub no_title: bool,

    /// Render at most this many seconds from each track.
    #[arg(long)]
    pub max_duration: Option<f64>,

    /// Process only the first N discovered tracks.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Print source/output mappings without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Refresh the summary without rendering video.
    #[arg(long, conflicts_with = "dry_run")]
    pub summary_only: bool,

    /// Suppress normal progress output.
    #[arg(short, long)]
    pub quiet: bool,
}
