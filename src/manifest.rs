use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::Read,
    path::Path,
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{batch::RenderOutcome, config::RenderConfig, discovery::Track};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Added,
    Modified,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Added => "ADDED",
            Self::Modified => "MODIFIED",
        })
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Changes {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub unchanged: usize,
}

impl Changes {
    pub fn record(&mut self, name: &str, change: Option<Change>) {
        match change {
            Some(Change::Added) => self.added.push(name.to_owned()),
            Some(Change::Modified) => self.modified.push(name.to_owned()),
            None => self.unchanged += 1,
        }
    }

    pub fn print(&self) {
        println!("{self}");
        for (change, names) in [
            (Change::Added, &self.added),
            (Change::Modified, &self.modified),
        ] {
            for name in names {
                println!("  [{change}] {name}");
            }
        }
    }
}

impl fmt::Display for Changes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ADDED {} | MODIFIED {} | UNCHANGED {}",
            self.added.len(),
            self.modified.len(),
            self.unchanged
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fingerprint {
    pub bytes: u64,
    pub modified_ns: u64,
}

impl Fingerprint {
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            bytes: metadata.len(),
            modified_ns: metadata.modified()?.duration_since(UNIX_EPOCH)?.as_nanos() as u64,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunProfile {
    pub visual_revision: u32,
    pub render: RenderConfig,
    pub max_duration_millis: Option<u64>,
}

impl RunProfile {
    pub fn new(render: RenderConfig, max_duration: Option<f64>) -> Self {
        Self {
            visual_revision: 1,
            render,
            max_duration_millis: max_duration.map(|v| (v * 1000.0).round() as u64),
        }
    }

    fn id(&self) -> Result<String> {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in serde_json::to_vec(self)? {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        Ok(format!("{hash:016x}"))
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Summary {
    schema: u32,
    #[serde(default)]
    pub changes: Changes,
    profile: Option<RunProfile>,
    tracks: Vec<TrackSummary>,
}

impl Default for Summary {
    fn default() -> Self {
        Self {
            schema: 1,
            changes: Changes::default(),
            profile: None,
            tracks: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct TrackSummary {
    source: String,
    title: String,
    output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    change: Option<Change>,
    status: TrackStatus,
    source_bytes: u64,
    source_modified_ns: u64,
    #[serde(default)]
    source_hash: String,
    profile_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TrackStatus {
    #[default]
    Pending,
    #[serde(alias = "unchanged")]
    Rendered,
    Failed,
}

impl Summary {
    pub fn load(path: &Path) -> Result<Self> {
        let summary: Self = load_json(path)?;
        ensure!(
            summary.schema == 1,
            "unsupported render summary schema {}",
            summary.schema
        );
        Ok(summary)
    }

    pub fn prepare(
        &mut self,
        selected: &[Track],
        root: &Path,
        profile: RunProfile,
        overwrite: bool,
        prune: bool,
    ) -> Result<Vec<Track>> {
        let profile_id = profile.id()?;
        let mut existing: BTreeMap<_, _> = self
            .tracks
            .drain(..)
            .map(|entry| (entry.source.clone(), entry))
            .collect();
        let mut seen = BTreeSet::new();
        let mut queue = Vec::new();
        self.changes = Changes::default();
        for track in selected {
            let source = portable_path(&track.source, root);
            let output = portable_path(&track.output, root);
            let key = source.clone();
            let fingerprint = Fingerprint::read(&track.source)?;
            let mut previous = existing.remove(&key);
            if previous.is_none()
                && track
                    .source
                    .extension()
                    .is_some_and(|extension| extension == "ogg")
            {
                previous =
                    existing.remove(&portable_path(&track.source.with_extension("wav"), root));
            }
            let same_stamp = previous.as_ref().is_some_and(|e| {
                e.source == source
                    && e.source_bytes == fingerprint.bytes
                    && e.source_modified_ns == fingerprint.modified_ns
            });
            let hash = match &previous {
                Some(entry) if same_stamp && !overwrite && !entry.source_hash.is_empty() => {
                    entry.source_hash.clone()
                }
                _ => hash_file(&track.source)?,
            };
            let changed = previous.as_ref().is_some_and(|entry| {
                if entry.source_hash.is_empty() {
                    !same_stamp
                } else {
                    entry.source_hash != hash
                }
            });
            let change = match &previous {
                None => Some(Change::Added),
                Some(entry)
                    if entry.status != TrackStatus::Rendered
                        && entry.change == Some(Change::Added) =>
                {
                    Some(Change::Added)
                }
                Some(_) if changed => Some(Change::Modified),
                Some(entry) if entry.status != TrackStatus::Rendered => entry.change,
                _ => None,
            };
            self.changes.record(&track.title, change);
            let current = !overwrite
                && !changed
                && track.output.is_file()
                && previous.as_ref().is_some_and(|entry| {
                    entry.output == output
                        && entry.profile_id == profile_id
                        && entry.status == TrackStatus::Rendered
                });
            let entry = TrackSummary {
                source,
                title: track.title.clone(),
                output,
                change,
                source_bytes: fingerprint.bytes,
                source_modified_ns: fingerprint.modified_ns,
                source_hash: hash,
                profile_id: profile_id.clone(),
                ..previous.filter(|_| current).unwrap_or_default()
            };
            if !current {
                queue.push(track.clone());
            }
            seen.insert(key.clone());
            existing.insert(key, entry);
        }
        if prune {
            existing.retain(|key, _| seen.contains(key));
        }
        self.profile = Some(profile);
        self.tracks = existing.into_values().collect();
        Ok(queue)
    }

    pub fn record(&mut self, outcome: &RenderOutcome, root: &Path) -> Result<()> {
        let source = portable_path(&outcome.track.source, root);
        let entry = self
            .tracks
            .iter_mut()
            .find(|entry| entry.source == source)
            .context("missing render entry")?;
        match &outcome.result {
            Ok(stats) => {
                entry.status = TrackStatus::Rendered;
                entry.duration_seconds = Some(round_seconds(stats.duration_seconds));
                entry.elapsed_seconds = Some(round_seconds(stats.elapsed_seconds));
                entry.output_bytes = Some(fs::metadata(&outcome.track.output)?.len());
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
        save_json(path, self)
    }
}

pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid summary {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0; 65536];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    Ok(hash.finalize().to_hex().to_string())
}

fn portable_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn round_seconds(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}
