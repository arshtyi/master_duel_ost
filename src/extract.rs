use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf, absolute},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    cli::ExtractArgs,
    fsb::write_ogg,
    manifest::{Change, Changes, Fingerprint, load_json, save_json},
    unity::{Bundle, Clip},
};

#[derive(Default, Deserialize, Serialize)]
struct Index {
    changes: Changes,
    #[serde(default)]
    errors: BTreeMap<String, String>,
    tracks: BTreeMap<String, AudioEntry>,
    bundles: BTreeMap<PathBuf, BundleEntry>,
}

#[derive(Deserialize, Serialize)]
struct AudioEntry {
    hash: String,
    file: String,
    #[serde(default)]
    bundle: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct BundleEntry {
    fingerprint: Fingerprint,
    tracks: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    resources: BTreeMap<PathBuf, Fingerprint>,
}

impl Index {
    fn export(
        &mut self,
        clip: &Clip,
        audio: &[u8],
        bundle: &Path,
        output: &Path,
    ) -> Result<Option<Change>> {
        let hash = blake3::hash(audio).to_hex().to_string();
        let change = match self.tracks.get(&clip.name) {
            None => Some(Change::Added),
            Some(entry) if entry.hash != hash => Some(Change::Modified),
            _ => None,
        };
        let file = format!("{}.ogg", clip.name);
        let destination = output.join(&file);
        if change.is_some() || !destination.is_file() {
            let temporary = destination.with_extension("ogg.tmp");
            if let Err(error) = write_ogg(audio, clip, BufWriter::new(File::create(&temporary)?)) {
                let _ = fs::remove_file(temporary);
                return Err(error).with_context(|| format!("export {}", clip.name));
            }
            fs::rename(temporary, destination)?;
        }
        self.changes.record(&clip.name, change);
        self.tracks.insert(
            clip.name.clone(),
            AudioEntry {
                hash,
                file,
                bundle: bundle.to_path_buf(),
            },
        );
        Ok(change)
    }
}

pub fn run(args: &ExtractArgs) -> Result<()> {
    let input = fs::canonicalize(&args.input)
        .with_context(|| format!("cannot open {}", args.input.display()))?;
    let output = absolute(&args.output_dir)?;
    fs::create_dir_all(&output)?;
    let mut index: Index = load_json(&args.summary)?;
    index.changes = Changes::default();
    index.errors.clear();
    let mut files = Vec::new();
    if input.join("LocalData").is_dir() {
        collect(&input.join("LocalData"), &mut files)?;
        for path in [
            "masterduel_Data/StreamingAssets",
            "masterduel_Data/data.unity3d",
        ] {
            let path = input.join(path);
            if path.exists() {
                collect(&path, &mut files)?;
            }
        }
    } else {
        collect(&input, &mut files)?;
    }
    files.sort();
    let started = Instant::now();
    let mut claimed = BTreeSet::new();
    let mut scanned = 0;
    let mut cached = 0;
    let mut bytes_read = 0;
    if !args.quiet {
        println!("Scanning {} files for BGM tracks...", files.len());
    }
    let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build()?;
    for (batch, chunk) in files.chunks(1024).enumerate() {
        let scanned_before = scanned;
        let results: Vec<_> = pool.install(|| {
            chunk
                .par_iter()
                .map(|path| scan(path, &index, args, &output))
                .collect()
        });
        for (path, scan_result) in chunk.iter().zip(results) {
            let result = (|| -> Result<()> {
                let Scan {
                    fingerprint,
                    clips,
                    bytes_read: read,
                } = scan_result?;
                bytes_read += read;
                let Some(clips) = clips else {
                    let entry = &index.bundles[path];
                    for name in &entry.tracks {
                        if claimed.insert(name.clone()) {
                            index.changes.record(name, None);
                        }
                    }
                    cached += 1;
                    return Ok(());
                };
                scanned += 1;
                let mut names = Vec::new();
                let mut resources = BTreeMap::new();
                if !clips.is_empty() {
                    let mut bundle = Bundle::open(path)?.context("bundle disappeared")?;
                    for clip in clips {
                        if let Some(path) = &clip.resource {
                            resources.insert(path.clone(), Fingerprint::read(path)?);
                        }
                        names.push(clip.name.clone());
                        if !claimed.insert(clip.name.clone()) {
                            continue;
                        }
                        let audio = bundle.audio(&clip)?;
                        let change = index.export(&clip, &audio, path, &output)?;
                        if !args.quiet
                            && let Some(change) = change
                        {
                            println!("[{change}] {}", clip.name);
                        }
                    }
                    bytes_read += bundle.bytes_read;
                }
                index.bundles.insert(
                    path.clone(),
                    BundleEntry {
                        fingerprint,
                        tracks: names,
                        resources,
                    },
                );
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("{}: {error:#}", path.display());
                index
                    .errors
                    .insert(path.to_string_lossy().into_owned(), format!("{error:#}"));
            }
        }
        if !args.quiet {
            println!(
                "[{}/{}] scanned {scanned} | cached {cached} | BGM {}",
                batch * 1024 + chunk.len(),
                files.len(),
                claimed.len()
            );
        }
        if scanned > scanned_before {
            save_json(&args.summary, &index)?;
        }
    }
    save_json(&args.summary, &index)?;
    if !args.quiet {
        println!("{}", index.changes);
        println!(
            "scanned {scanned} | cached {cached} | read {:.1} MiB | {:.2}s",
            bytes_read as f64 / 1048576.0,
            started.elapsed().as_secs_f64()
        );
        println!("summary: {}", args.summary.display());
    }
    if !index.errors.is_empty() {
        bail!(
            "{} bundle(s) failed; see {}",
            index.errors.len(),
            args.summary.display()
        );
    }
    Ok(())
}

struct Scan {
    fingerprint: Fingerprint,
    clips: Option<Vec<Clip>>,
    bytes_read: u64,
}

fn scan(path: &Path, index: &Index, args: &ExtractArgs, output: &Path) -> Result<Scan> {
    let fingerprint = Fingerprint::read(path)?;
    if !args.rescan
        && let Some(entry) = index.bundles.get(path)
        && entry.fingerprint == fingerprint
        && entry
            .resources
            .iter()
            .all(|(path, stamp)| Fingerprint::read(path).is_ok_and(|current| current == *stamp))
        && entry.tracks.iter().all(|name| {
            index
                .tracks
                .get(name)
                .is_some_and(|track| track.bundle == path && output.join(&track.file).is_file())
        })
    {
        return Ok(Scan {
            fingerprint,
            clips: None,
            bytes_read: 0,
        });
    }
    let (clips, bytes_read) = if let Some(mut bundle) = Bundle::open(path)? {
        (bundle.music()?, bundle.bytes_read)
    } else {
        (Vec::new(), 8)
    };
    Ok(Scan {
        fingerprint,
        clips: Some(clips),
        bytes_read,
    })
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}
