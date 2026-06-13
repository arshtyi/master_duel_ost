use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    audio::estimate_duration, batch::RenderOutcome, config::RenderConfig, discovery::Track,
};

const SCHEMA_VERSION: u32 = 1;
const CURRENT_VISUAL_REVISION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunProfile {
    #[serde(default = "current_visual_revision")]
    pub visual_revision: u32,
    pub render: RenderConfig,
    pub max_duration_millis: Option<u64>,
}

impl RunProfile {
    pub fn new(render: RenderConfig, max_duration: Option<f64>) -> Self {
        Self {
            visual_revision: CURRENT_VISUAL_REVISION,
            render,
            max_duration_millis: max_duration.map(|value| (value * 1_000.0).round() as u64),
        }
    }

    fn id(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self).context("failed to fingerprint render profile")?;
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Ok(format!("{hash:016x}"))
    }
}

fn current_visual_revision() -> u32 {
    CURRENT_VISUAL_REVISION
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Summary {
    schema: u32,
    profile: Option<RunProfile>,
    tracks: Vec<TrackSummary>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TrackSummary {
    source: String,
    title: String,
    output: String,
    source_bytes: u64,
    source_modified_ns: u64,
    profile_id: String,
    status: TrackStatus,
    duration_seconds: Option<f64>,
    elapsed_seconds: Option<f64>,
    output_bytes: Option<u64>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TrackStatus {
    Pending,
    Rendered,
    Unchanged,
    Failed,
}

impl Default for Summary {
    fn default() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            profile: None,
            tracks: Vec::new(),
        }
    }
}

impl Summary {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes =
            fs::read(path).with_context(|| format!("failed to read summary {}", path.display()))?;
        let summary: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid summary {}", path.display()))?;
        if summary.schema != SCHEMA_VERSION {
            bail!(
                "unsupported summary schema {} in {}",
                summary.schema,
                path.display()
            );
        }
        Ok(summary)
    }

    pub fn prepare(
        &mut self,
        selected: &[Track],
        project_root: &Path,
        profile: RunProfile,
        overwrite: bool,
        prune_missing: bool,
    ) -> Result<Vec<Track>> {
        let profile_id = profile.id()?;
        let duration_limit = profile
            .max_duration_millis
            .map(|milliseconds| milliseconds as f64 / 1_000.0);
        let mut existing: BTreeMap<String, TrackSummary> = self
            .tracks
            .drain(..)
            .map(|entry| (entry.source.clone(), entry))
            .collect();
        let mut seen = BTreeSet::new();
        let mut queue = Vec::new();

        for track in selected {
            let source = portable_path(&track.source, project_root);
            let output = portable_path(&track.output, project_root);
            let fingerprint = source_fingerprint(&track.source)?;
            let estimated_duration = estimate_duration(&track.source)
                .map(|duration| duration_limit.map_or(duration, |limit| duration.min(limit)))
                .map(round_seconds);
            seen.insert(source.clone());
            let previous = existing.remove(&source);
            let unchanged = !overwrite
                && track.output.is_file()
                && previous.as_ref().is_some_and(|entry| {
                    entry.source_bytes == fingerprint.0
                        && entry.source_modified_ns == fingerprint.1
                        && entry.profile_id == profile_id
                        && matches!(entry.status, TrackStatus::Rendered | TrackStatus::Unchanged)
                });

            if unchanged {
                let mut entry = previous.expect("checked above");
                entry.title.clone_from(&track.title);
                entry.output = output;
                entry.status = TrackStatus::Unchanged;
                entry.output_bytes = fs::metadata(&track.output)
                    .ok()
                    .map(|metadata| metadata.len());
                entry.error = None;
                existing.insert(source, entry);
            } else {
                existing.insert(
                    source.clone(),
                    TrackSummary {
                        source,
                        title: track.title.clone(),
                        output,
                        source_bytes: fingerprint.0,
                        source_modified_ns: fingerprint.1,
                        profile_id: profile_id.clone(),
                        status: TrackStatus::Pending,
                        duration_seconds: estimated_duration,
                        elapsed_seconds: None,
                        output_bytes: None,
                        error: None,
                    },
                );
                queue.push(track.clone());
            }
        }

        if prune_missing {
            existing.retain(|source, _| seen.contains(source));
        }
        self.schema = SCHEMA_VERSION;
        self.profile = Some(profile);
        self.tracks = existing.into_values().collect();
        Ok(queue)
    }

    pub fn record(&mut self, outcome: &RenderOutcome, project_root: &Path) -> Result<()> {
        let source = portable_path(&outcome.track.source, project_root);
        let entry = self
            .tracks
            .iter_mut()
            .find(|entry| entry.source == source)
            .with_context(|| format!("summary entry disappeared for {source}"))?;
        match &outcome.result {
            Ok(stats) => {
                entry.status = TrackStatus::Rendered;
                entry.duration_seconds = Some(round_seconds(stats.duration_seconds));
                entry.elapsed_seconds = Some(round_seconds(stats.elapsed_seconds));
                entry.output_bytes = Some(
                    fs::metadata(&outcome.track.output)
                        .with_context(|| {
                            format!("failed to inspect {}", outcome.track.output.display())
                        })?
                        .len(),
                );
                entry.error = None;
            }
            Err(error) => {
                entry.status = TrackStatus::Failed;
                entry.error = Some(error.clone());
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let temporary = temporary_summary_path(path);
        let mut bytes = serde_json::to_vec_pretty(self).context("failed to serialize summary")?;
        bytes.push(b'\n');
        fs::write(&temporary, bytes)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace summary {}", path.display()))
    }
}

fn source_fingerprint(path: &Path) -> Result<(u64, u64)> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect source {}", path.display()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    Ok((metadata.len(), modified))
}

fn portable_path(path: &Path, project_root: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn temporary_summary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    name.push_str(".tmp");
    path.with_file_name(name)
}

fn round_seconds(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_id_changes_with_visual_settings() {
        let mut render = RenderConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            visual_fps: 24,
            bars: 48,
            analysis_sample_rate: 12_000,
            fft_size: 2_048,
            encoder: "libopenh264".into(),
            quality: 18,
            preset: None,
            audio_bitrate: "320k".into(),
            show_title: true,
        };
        let first = RunProfile::new(render.clone(), None).id().unwrap();
        render.bars = 64;
        let second = RunProfile::new(render, None).id().unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn profile_id_changes_with_visual_revision() {
        let render = RenderConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            visual_fps: 24,
            bars: 48,
            analysis_sample_rate: 12_000,
            fft_size: 2_048,
            encoder: "libopenh264".into(),
            quality: 18,
            preset: None,
            audio_bitrate: "320k".into(),
            show_title: true,
        };
        let current = RunProfile::new(render, None);
        let mut previous = current.clone();
        previous.visual_revision -= 1;

        assert_ne!(current.id().unwrap(), previous.id().unwrap());
    }

    #[test]
    fn summary_status_uses_stable_names() {
        assert_eq!(
            serde_json::to_string(&TrackStatus::Unchanged).unwrap(),
            "\"unchanged\""
        );
    }
}
