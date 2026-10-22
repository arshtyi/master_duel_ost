use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use anyhow::{Context, Result};

use crate::{
    config::RenderConfig,
    discovery::Track,
    encoder::{RenderProgress, RenderStats, render_track},
};

pub struct RenderOutcome {
    pub track: Track,
    pub result: Result<RenderStats, String>,
}

enum Event {
    Progress(usize, RenderProgress),
    Finished(usize, RenderOutcome),
}

pub fn render_batch<F>(
    tracks: Vec<Track>,
    config: &RenderConfig,
    max_duration: Option<f64>,
    jobs: usize,
    encoder_threads: usize,
    quiet: bool,
    mut on_outcome: F,
) -> Result<usize>
where
    F: FnMut(&RenderOutcome) -> Result<()>,
{
    let total = tracks.len();
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    let terminal = !quiet && io::stdout().is_terminal();
    if !quiet {
        println!(
            "render: {total} track(s) | {jobs} jobs | {}x{}@{} | {}",
            config.width, config.height, config.fps, config.encoder
        );
    }
    thread::scope(|scope| -> Result<usize> {
        for _ in 0..jobs {
            let sender = sender.clone();
            let tracks = &tracks;
            let next = &next;
            scope.spawn(move || {
                while let Some((index, track)) = {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    tracks.get(index).map(|track| (index, track))
                } {
                    let result =
                        render_track(track, config, max_duration, encoder_threads, |progress| {
                            let _ = sender.send(Event::Progress(index, progress));
                        })
                        .map_err(|error| format!("{error:#}"));
                    let outcome = RenderOutcome {
                        track: track.clone(),
                        result,
                    };
                    if sender.send(Event::Finished(index, outcome)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        let mut active = BTreeMap::<usize, RenderProgress>::new();
        let mut completed = 0;
        let mut failures = 0;
        let mut save_error = None;
        let mut last_draw = Instant::now();
        for event in receiver {
            match event {
                Event::Progress(index, progress) => {
                    if !quiet && !terminal && progress.media_seconds == 0.0 {
                        println!(
                            "[{}/{}] start: {} ({:.1}s)",
                            index + 1,
                            total,
                            tracks[index].title,
                            progress.duration_seconds
                        );
                    }
                    active.insert(index, progress);
                }
                Event::Finished(index, outcome) => {
                    active.remove(&index);
                    completed += 1;
                    if terminal {
                        print!("\r\x1b[2K");
                    }
                    match &outcome.result {
                        Ok(stats) if !quiet => println!(
                            "[{completed}/{total}] done: {} | {:.1}s / {:.1}s",
                            outcome.track.title, stats.elapsed_seconds, stats.duration_seconds
                        ),
                        Err(error) => {
                            failures += 1;
                            eprintln!(
                                "[{completed}/{total}] failed: {}: {error}",
                                outcome.track.title
                            );
                        }
                        _ => {}
                    }
                    if save_error.is_none()
                        && let Err(error) = on_outcome(&outcome)
                    {
                        save_error = Some(error);
                    }
                }
            }
            if terminal && last_draw.elapsed().as_millis() >= 400 {
                let progress = active
                    .iter()
                    .map(|(&index, p)| {
                        format!(
                            "{} {:.0}%",
                            tracks[index].title,
                            p.media_seconds / p.duration_seconds * 100.0
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                print!("\r\x1b[2K[{completed}/{total}] {progress}");
                let _ = io::stdout().flush();
                last_draw = Instant::now();
            }
        }
        if terminal {
            print!("\r\x1b[2K");
            let _ = io::stdout().flush();
        }
        if let Some(error) = save_error {
            return Err(error).context("failed to update render summary");
        }
        Ok(failures)
    })
}
