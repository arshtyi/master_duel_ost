use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use rustfft::{FftPlanner, num_complex::Complex32};

const MIN_FREQUENCY: f32 = 40.0;
const PEAK_HISTOGRAM_BINS: usize = 8_192;
const TEMPORAL_ATTACK: f32 = 0.58;
const TEMPORAL_RELEASE: f32 = 0.55;

pub fn estimate_duration(source: &Path) -> Option<f64> {
    if source
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
        && let Ok(mut file) = File::open(source)
        && let Some(duration) = wav_duration(&mut file)
    {
        return Some(duration);
    }

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(source)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|duration| duration.is_finite() && *duration > 0.0)
}

fn wav_duration(reader: &mut (impl Read + Seek)) -> Option<f64> {
    let mut header = [0_u8; 12];
    reader.read_exact(&mut header).ok()?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return None;
    }

    let mut byte_rate = None;
    let mut data_bytes = None;
    loop {
        let mut chunk_header = [0_u8; 8];
        if reader.read_exact(&mut chunk_header).is_err() {
            break;
        }
        let chunk_size = u32::from_le_bytes(chunk_header[4..].try_into().ok()?) as u64;
        match &chunk_header[..4] {
            b"fmt " if chunk_size >= 12 => {
                let mut format = [0_u8; 12];
                reader.read_exact(&mut format).ok()?;
                byte_rate = Some(u32::from_le_bytes(format[8..12].try_into().ok()?));
                reader
                    .seek(SeekFrom::Current((chunk_size - 12 + chunk_size % 2) as i64))
                    .ok()?;
            }
            b"data" => {
                data_bytes = Some(chunk_size);
                if byte_rate.is_some() {
                    break;
                }
                reader
                    .seek(SeekFrom::Current((chunk_size + chunk_size % 2) as i64))
                    .ok()?;
            }
            _ => {
                reader
                    .seek(SeekFrom::Current((chunk_size + chunk_size % 2) as i64))
                    .ok()?;
            }
        }
    }
    let byte_rate = f64::from(byte_rate?);
    let duration = data_bytes? as f64 / byte_rate;
    (byte_rate > 0.0 && duration.is_finite() && duration > 0.0).then_some(duration)
}

#[derive(Debug)]
pub struct AudioAnalysis {
    pub duration: f64,
    spectrum: Vec<f32>,
    energy: Vec<f32>,
    frame_rate: f32,
    bars: usize,
}

impl AudioAnalysis {
    pub fn decode(
        source: &Path,
        sample_rate: u32,
        visual_fps: u32,
        bars: usize,
        fft_size: usize,
        max_duration: Option<f64>,
    ) -> Result<Self> {
        let mut command = Command::new("ffmpeg");
        command
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(source)
            .args(["-map", "0:a:0", "-vn", "-ac", "1", "-ar"])
            .arg(sample_rate.to_string());
        if let Some(duration) = max_duration {
            command.args(["-t", &format!("{duration:.6}")]);
        }
        let output = command
            .args(["-f", "s16le", "pipe:1"])
            .stdin(Stdio::null())
            .output()
            .with_context(|| "failed to launch ffmpeg for audio analysis")?;
        if !output.status.success() {
            bail!(
                "ffmpeg could not decode {}: {}",
                source.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if output.stdout.len() < 2 {
            bail!("audio stream is empty: {}", source.display());
        }

        let decoded = output.stdout;
        let mut samples = Vec::with_capacity(decoded.len() / 2);
        for bytes in decoded.chunks_exact(2) {
            let value = i16::from_le_bytes(bytes.try_into().expect("two-byte chunk"));
            samples.push(f32::from(value) / 32_768.0);
        }
        drop(decoded);
        normalize_samples(&mut samples);

        let duration = samples.len() as f64 / sample_rate as f64;
        let frame_rate = visual_fps as f32;
        let energy = rms_envelope(&samples, sample_rate, frame_rate, 0.060, 0.95);
        let spectrum =
            frequency_spectrum(&samples, sample_rate, frame_rate, bars, fft_size, &energy);
        Ok(Self {
            duration,
            spectrum,
            energy,
            frame_rate,
            bars,
        })
    }

    pub fn energy_at(&self, time: f32) -> f32 {
        sample_linear(&self.energy, time * self.frame_rate).clamp(0.0, 1.0)
    }

    pub fn spectrum_at(&self, time: f32) -> &[f32] {
        let frames = self.spectrum.len() / self.bars;
        let frame = (time * self.frame_rate).round() as usize;
        let start = frame.min(frames.saturating_sub(1)) * self.bars;
        &self.spectrum[start..start + self.bars]
    }
}

fn frequency_spectrum(
    samples: &[f32],
    sample_rate: u32,
    frame_rate: f32,
    bars: usize,
    fft_size: usize,
    energy: &[f32],
) -> Vec<f32> {
    let frame_count =
        ((samples.len() as f64 / sample_rate as f64) * frame_rate as f64).ceil() as usize + 2;
    let band_count = bars / 2;
    let ranges = logarithmic_band_ranges(sample_rate, fft_size, band_count);
    let frequency_weights: Vec<f32> = ranges
        .iter()
        .map(|range| {
            let frequency =
                (range.start + range.end) as f32 * 0.5 * sample_rate as f32 / fft_size as f32;
            (frequency / 80.0).max(1.0).powf(0.34)
        })
        .collect();
    let window: Vec<f32> = (0..fft_size)
        .map(|index| {
            0.5 - 0.5 * (std::f32::consts::TAU * index as f32 / (fft_size - 1) as f32).cos()
        })
        .collect();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut buffer = vec![Complex32::default(); fft_size];
    let mut bands = vec![0.0_f32; frame_count * band_count];

    for frame in 0..frame_count {
        let center = (frame as f32 * sample_rate as f32 / frame_rate).round() as isize;
        let first_sample = center - fft_size as isize / 2;
        buffer.fill(Complex32::default());
        let source_start = first_sample.max(0) as usize;
        let window_start = (source_start as isize - first_sample) as usize;
        if source_start < samples.len() && window_start < fft_size {
            let copy_length = (samples.len() - source_start).min(fft_size - window_start);
            for index in 0..copy_length {
                buffer[window_start + index].re =
                    samples[source_start + index] * window[window_start + index];
            }
        }
        fft.process(&mut buffer);

        for (band, range) in ranges.iter().enumerate() {
            let power: f32 = buffer[range.clone()].iter().map(Complex32::norm_sqr).sum();
            bands[frame * band_count + band] =
                (power / range.len() as f32).sqrt() * frequency_weights[band];
        }
    }

    normalize_spectrum(&mut bands);
    smooth_and_mirror_spectrum(&bands, energy, frame_count, band_count, bars)
}

fn logarithmic_band_ranges(
    sample_rate: u32,
    fft_size: usize,
    band_count: usize,
) -> Vec<std::ops::Range<usize>> {
    let nyquist = sample_rate as f32 * 0.5;
    let max_frequency = nyquist * 0.92;
    let ratio = (max_frequency / MIN_FREQUENCY).powf(1.0 / band_count as f32);
    let max_bin = fft_size / 2;
    let mut ranges = Vec::with_capacity(band_count);
    let mut start = ((MIN_FREQUENCY * fft_size as f32 / sample_rate as f32).floor() as usize)
        .clamp(1, max_bin.saturating_sub(1));

    for band in 0..band_count {
        let upper_frequency = MIN_FREQUENCY * ratio.powi((band + 1) as i32);
        let mut end = (upper_frequency * fft_size as f32 / sample_rate as f32).ceil() as usize;
        end = end.clamp(start + 1, max_bin);
        ranges.push(start..end);
        start = end.min(max_bin.saturating_sub(1));
    }
    ranges
}

fn normalize_spectrum(values: &mut [f32]) {
    let mut ranked = values.to_vec();
    let scale = percentile(&mut ranked, 0.985);
    if scale <= 1.0e-8 {
        return;
    }
    for value in values {
        let normalized = (*value / scale - 0.025).max(0.0) / 0.975;
        *value = normalized.clamp(0.0, 1.0).powf(0.68);
    }
}

fn smooth_and_mirror_spectrum(
    input: &[f32],
    energy: &[f32],
    frame_count: usize,
    band_count: usize,
    bars: usize,
) -> Vec<f32> {
    let mut output = vec![0.0; frame_count * bars];
    let mut previous = vec![0.0; band_count];
    for frame in 0..frame_count {
        let source = &input[frame * band_count..(frame + 1) * band_count];
        let loudness = 0.34 + energy.get(frame).copied().unwrap_or_default().sqrt() * 0.66;
        for band in 0..band_count {
            let left = source[band.saturating_sub(1)];
            let right = source[(band + 1).min(band_count - 1)];
            let spatial = (source[band] * 0.72 + (left + right) * 0.14) * loudness;
            previous[band] = smooth_spectrum_level(previous[band], spatial);
        }
        let target = &mut output[frame * bars..(frame + 1) * bars];
        for index in 0..band_count {
            target[index] = previous[band_count - 1 - index];
            target[band_count + index] = previous[index];
        }
    }
    output
}

fn smooth_spectrum_level(previous: f32, target: f32) -> f32 {
    let alpha = if target >= previous {
        TEMPORAL_ATTACK
    } else {
        TEMPORAL_RELEASE
    };
    previous + (target - previous) * alpha
}

fn normalize_samples(samples: &mut [f32]) {
    let peak = absolute_percentile(samples, 0.995);
    if peak > 1.0e-6 {
        for sample in samples {
            *sample = (*sample / peak).clamp(-1.0, 1.0);
        }
    }
}

fn rms_envelope(
    samples: &[f32],
    sample_rate: u32,
    frame_rate: f32,
    half_window_seconds: f32,
    percentile_rank: f32,
) -> Vec<f32> {
    let frame_count =
        ((samples.len() as f64 / sample_rate as f64) * frame_rate as f64).ceil() as usize + 2;
    let half_window = ((sample_rate as f32 * half_window_seconds).round() as usize).max(1);
    let mut result = Vec::with_capacity(frame_count);
    let mut window_start = 0;
    let mut window_end = 0;
    let mut square_sum = 0.0_f64;
    for frame in 0..frame_count {
        let center = (frame as f32 * sample_rate as f32 / frame_rate).round() as usize;
        let start = center.saturating_sub(half_window).min(samples.len());
        let end = center.saturating_add(half_window).min(samples.len());
        while window_end < end {
            square_sum += f64::from(samples[window_end]).powi(2);
            window_end += 1;
        }
        while window_start < start {
            square_sum -= f64::from(samples[window_start]).powi(2);
            window_start += 1;
        }
        let length = end.saturating_sub(start).max(1);
        result.push((square_sum.max(0.0) / length as f64).sqrt() as f32);
    }

    let mut ranked = result.clone();
    let scale = percentile(&mut ranked, percentile_rank);
    if scale > 1.0e-8 {
        for value in &mut result {
            *value = (*value / scale).clamp(0.0, 1.0);
        }
    }
    result
}

fn absolute_percentile(samples: &[f32], rank: f32) -> f32 {
    let maximum = samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0_f32, f32::max);
    if samples.is_empty() || maximum <= f32::EPSILON {
        return maximum;
    }

    let mut histogram = [0_u32; PEAK_HISTOGRAM_BINS];
    let bin_scale = (PEAK_HISTOGRAM_BINS - 1) as f32 / maximum;
    for sample in samples {
        let bin = (sample.abs() * bin_scale) as usize;
        histogram[bin.min(PEAK_HISTOGRAM_BINS - 1)] += 1;
    }
    let target = ((samples.len() - 1) as f32 * rank.clamp(0.0, 1.0)).round() as usize;
    let mut seen = 0_usize;
    for (bin, count) in histogram.into_iter().enumerate() {
        seen += count as usize;
        if seen > target {
            return (((bin + 1).min(PEAK_HISTOGRAM_BINS - 1)) as f32 / bin_scale).min(maximum);
        }
    }
    maximum
}

fn percentile(values: &mut [f32], rank: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let index = ((values.len() - 1) as f32 * rank.clamp(0.0, 1.0)).round() as usize;
    let (_, value, _) = values.select_nth_unstable_by(index, |left, right| left.total_cmp(right));
    *value
}

fn sample_linear(values: &[f32], position: f32) -> f32 {
    if values.is_empty() || position < 0.0 {
        return 0.0;
    }
    let index = position.floor() as usize;
    let Some(left) = values.get(index) else {
        return 0.0;
    };
    let right = values.get(index + 1).copied().unwrap_or(*left);
    left + (right - left) * position.fract()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn envelope_tracks_louder_samples() {
        let mut samples = vec![0.01; 4_000];
        samples.extend(vec![0.8; 4_000]);
        let envelope = rms_envelope(&samples, 8_000, 20.0, 0.02, 0.95);
        assert!(envelope[15] > envelope[2] * 10.0);
    }

    #[test]
    fn rolling_envelope_matches_prefix_reference() {
        let samples: Vec<f32> = (0..2_000)
            .map(|index| ((index % 37) as f32 - 18.0) / 18.0)
            .collect();
        let actual = rms_envelope(&samples, 1_000, 10.0, 0.02, 0.95);
        let mut prefix = vec![0.0_f64];
        for sample in &samples {
            prefix.push(prefix.last().copied().unwrap() + f64::from(*sample).powi(2));
        }
        let mut expected: Vec<f32> = (0..actual.len())
            .map(|frame| {
                let center = frame * 100;
                let start = center.saturating_sub(20).min(samples.len());
                let end = center.saturating_add(20).min(samples.len());
                ((prefix[end] - prefix[start]) / end.saturating_sub(start).max(1) as f64).sqrt()
                    as f32
            })
            .collect();
        let mut ranked = expected.clone();
        let scale = percentile(&mut ranked, 0.95);
        for value in &mut expected {
            *value = (*value / scale).clamp(0.0, 1.0);
        }

        for (actual, expected) in actual.iter().zip(expected) {
            assert!((actual - expected).abs() < 1.0e-5);
        }
    }

    #[test]
    fn spectrum_places_high_tones_above_low_tones() {
        let sample_rate = 12_000;
        let frame_rate = 24.0;
        let fft_size = 2_048;
        let bars = 48;
        let make_tone = |frequency: f32| {
            (0..sample_rate)
                .map(|index| {
                    (std::f32::consts::TAU * frequency * index as f32 / sample_rate as f32).sin()
                })
                .collect::<Vec<_>>()
        };
        let energy = vec![1.0; frame_rate as usize + 2];
        let low = frequency_spectrum(
            &make_tone(100.0),
            sample_rate,
            frame_rate,
            bars,
            fft_size,
            &energy,
        );
        let high = frequency_spectrum(
            &make_tone(3_000.0),
            sample_rate,
            frame_rate,
            bars,
            fft_size,
            &energy,
        );
        let frame = 12;
        let half = bars / 2;
        let dominant = |spectrum: &[f32]| {
            spectrum[frame * bars..frame * bars + half]
                .iter()
                .enumerate()
                .max_by(|left, right| left.1.total_cmp(right.1))
                .unwrap()
                .0
        };

        assert!(dominant(&low) > dominant(&high) + 8);
    }

    #[test]
    fn spectrum_releases_old_peaks_quickly() {
        let mut level = 1.0;
        for _ in 0..4 {
            level = smooth_spectrum_level(level, 0.0);
        }

        assert!(level < 0.05);
    }

    #[test]
    fn percentile_is_deterministic() {
        let mut values = vec![9.0, 1.0, 5.0, 3.0];
        assert_eq!(percentile(&mut values, 0.5), 5.0);
    }

    #[test]
    fn reads_duration_from_pcm_wav_header() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&32_036_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&32_000_u32.to_le_bytes());
        wav.extend_from_slice(&4_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&32_000_u32.to_le_bytes());
        wav.resize(32_044, 0);

        assert_eq!(wav_duration(&mut Cursor::new(wav)), Some(1.0));
    }
}
