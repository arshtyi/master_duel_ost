mod audio;
mod batch;
mod cli;
mod config;
mod discovery;
mod encoder;
mod extract;
mod fsb;
mod manifest;
mod renderer;
mod unity;

use std::path::absolute;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::{
    batch::render_batch,
    cli::{Cli, RenderArgs, Task},
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
    match Cli::parse().command {
        Task::Extract(args) => extract::run(&args),
        Task::Render(args) => render(args),
    }
}

fn render(cli: RenderArgs) -> Result<()> {
    let mut config = RenderConfig::try_from(&cli)?;
    let input = absolute(&cli.input)?;
    let output_dir = absolute(&cli.output_dir)?;
    let mut tracks = discover_tracks(&input, &output_dir)?;

    if tracks.is_empty() {
        bail!("no supported audio files found in {}", input.display());
    }
    if let Some(limit) = cli.limit {
        tracks.truncate(limit);
    }

    if cli.dry_run {
        for (index, track) in tracks.iter().enumerate() {
            println!(
                "[{}/{}] {} -> {} ({})",
                index + 1,
                tracks.len(),
                track.source.display(),
                track.output.display(),
                track.title
            );
        }
        return Ok(());
    }

    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    let project_root = std::env::current_dir().context("failed to resolve current directory")?;
    let summary_path = absolute(&cli.summary)?;
    let profile = RunProfile::new(config.clone(), cli.max_duration);
    let mut summary = Summary::load(&summary_path)?;
    let render_queue = summary.prepare(
        &tracks,
        &project_root,
        profile,
        cli.overwrite,
        cli.limit.is_none() && input.is_dir(),
    )?;
    summary.save(&summary_path)?;

    if !cli.quiet {
        summary.changes.print();
    }

    if cli.summary_only {
        if !cli.quiet {
            println!(
                "indexed {} track(s) in {} without rendering",
                tracks.len(),
                summary_path.display()
            );
        }
        return Ok(());
    }

    if render_queue.is_empty() {
        if !cli.quiet {
            println!("all outputs are up to date");
        }
        return Ok(());
    }

    config.encoder = resolve_encoder(&config.encoder)?;
    let workers = cli.jobs.min(render_queue.len());
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
