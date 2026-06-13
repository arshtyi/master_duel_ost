mod audio;
mod batch;
mod cli;
mod config;
mod discovery;
mod encoder;
mod manifest;
mod renderer;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::{
    batch::render_batch,
    cli::Cli,
    config::RenderConfig,
    discovery::discover_tracks,
    encoder::resolve_encoder,
    manifest::{RunProfile, Summary},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut config = RenderConfig::try_from(&cli)?;
    let input = absolute(&cli.input)?;
    let output_dir = absolute(&cli.output_dir)?;
    let tracks = discover_tracks(&input, &output_dir)?;

    if tracks.is_empty() {
        bail!("no supported audio files found in {}", input.display());
    }
    let mut selected_tracks = tracks;
    if let Some(limit) = cli.limit {
        selected_tracks.truncate(limit);
    }

    if cli.dry_run {
        for (index, track) in selected_tracks.iter().enumerate() {
            println!(
                "[{}/{}] {} -> {} ({})",
                index + 1,
                selected_tracks.len(),
                track.source.display(),
                track.output.display(),
                track.title
            );
        }
        return Ok(());
    }

    config.encoder = resolve_encoder(&config.encoder)?;
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    let project_root = std::env::current_dir().context("failed to resolve current directory")?;
    let summary_path = absolute(&cli.summary)?;
    let profile = RunProfile::new(config.clone(), cli.max_duration);
    let mut summary = Summary::load(&summary_path)?;
    let render_queue = summary.prepare(
        &selected_tracks,
        &project_root,
        profile,
        cli.overwrite,
        cli.limit.is_none(),
    )?;
    summary.save(&summary_path)?;

    if cli.summary_only {
        if !cli.quiet {
            println!(
                "indexed {} track(s) in {} without rendering",
                selected_tracks.len(),
                summary_path.display()
            );
        }
        return Ok(());
    }

    let unchanged = selected_tracks.len() - render_queue.len();
    if unchanged > 0 && !cli.quiet {
        println!("unchanged: {unchanged}");
    }
    if render_queue.is_empty() {
        if !cli.quiet {
            println!("all outputs are up to date");
        }
        return Ok(());
    }

    let workers = cli.jobs.min(render_queue.len()).max(1);
    let threads = encoder_threads(workers);
    let failures = render_batch(
        render_queue,
        &config,
        cli.max_duration,
        workers,
        threads,
        cli.quiet,
        |outcome| {
            summary.record(outcome, &project_root)?;
            summary.save(&summary_path)
        },
    )?;

    if !cli.quiet {
        println!("summary updated: {}", summary_path.display());
    }
    if failures > 0 {
        bail!(
            "{failures} track(s) failed; details were saved to {}",
            summary_path.display()
        );
    }
    Ok(())
}

fn encoder_threads(jobs: usize) -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .saturating_sub(jobs)
        .div_ceil(jobs)
        .max(1)
}

fn absolute(path: &std::path::Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current directory")?
        .join(path))
}
