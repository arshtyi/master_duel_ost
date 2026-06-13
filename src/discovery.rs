use std::{
    collections::HashSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "flac", "m4a", "mp3", "ogg", "opus", "wav", "wma",
];

#[derive(Clone, Debug)]
pub struct Track {
    pub source: PathBuf,
    pub output: PathBuf,
    pub title: String,
}

pub fn discover_tracks(input: &Path, output_dir: &Path) -> Result<Vec<Track>> {
    let mut files = Vec::new();
    if input.is_file() {
        if !is_audio(input) {
            bail!("unsupported audio extension: {}", input.display());
        }
        files.push(input.to_path_buf());
    } else if input.is_dir() {
        collect_directory(input, &mut files)?;
    } else {
        bail!("input path does not exist: {}", input.display());
    }

    files.sort_by(|left, right| {
        sort_name(left)
            .cmp(&sort_name(right))
            .then_with(|| left.cmp(right))
    });

    let mut used = HashSet::with_capacity(files.len());
    let mut tracks = Vec::with_capacity(files.len());
    for source in files {
        let title = title_from_path(&source);
        let base = safe_video_stem(&title);
        let mut stem = base.clone();
        let mut suffix = 2;
        while !used.insert(stem.to_lowercase()) {
            stem = format!("{base} {suffix}");
            suffix += 1;
        }
        tracks.push(Track {
            source,
            output: output_dir.join(format!("{stem}.mp4")),
            title,
        });
    }
    Ok(tracks)
}

fn collect_directory(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if file_type.is_dir() {
            collect_directory(&path, files)?;
        } else if file_type.is_file() && is_audio(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
}

fn sort_name(path: &Path) -> String {
    path.file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_lowercase()
}

pub fn title_from_path(path: &Path) -> String {
    let original = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
    let mut name = original;
    if name
        .as_bytes()
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"BGM"))
    {
        name = name.get(3..).unwrap_or_default();
    }
    name = name.trim_start_matches(['_', ' ', '-']);
    let normalized = name
        .to_lowercase()
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        original.to_lowercase()
    } else {
        normalized
    }
}

pub fn safe_video_stem(title: &str) -> String {
    let filtered: String = title
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
        .collect();
    let stem = filtered.split_whitespace().collect::<Vec<_>>().join(" ");
    let stem = stem.trim_matches([' ', '.']);
    if stem.is_empty() {
        "untitled".to_owned()
    } else {
        stem.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_python_title_rules() {
        assert_eq!(
            title_from_path(Path::new("BGM_DUEL_CLIMAX_01.wav")),
            "duel climax 01"
        );
        assert_eq!(
            title_from_path(Path::new("bgm-menu__retro.wav")),
            "menu retro"
        );
        assert_eq!(title_from_path(Path::new("SONG.WAV")), "song");
    }

    #[test]
    fn sanitizes_video_stems() {
        assert_eq!(safe_video_stem("a:  b?/c"), "a bc");
        assert_eq!(safe_video_stem("..."), "untitled");
    }
}
