use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, IsTerminal, Write},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Instant,
};

use anyhow::{Context, Result};

use crate::{
    audio::estimate_duration,
    config::RenderConfig,
    discovery::Track,
    encoder::{RenderProgress, RenderStats, render_track},
};

pub struct RenderOutcome {
    pub track: Track,
    pub result: Result<RenderStats, String>,
}

enum BatchEvent {
    Started {
        index: usize,
    },
    Progress {
        index: usize,
        progress: RenderProgress,
    },
    Finished {
        index: usize,
        outcome: RenderOutcome,
    },
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
    let workers = jobs.min(total).max(1);
    let titles: Vec<String> = tracks.iter().map(|track| track.title.clone()).collect();
    let expected_durations: Vec<f64> = tracks
        .iter()
        .map(|track| limited_duration(estimate_duration(&track.source), max_duration))
        .collect();
    let mut display = BatchDisplay::new(
        titles,
        expected_durations,
        workers,
        encoder_threads,
        config,
        quiet,
    );
    let queue = Arc::new(Mutex::new(
        tracks.into_iter().enumerate().collect::<VecDeque<_>>(),
    ));
    let (sender, receiver) = mpsc::channel();

    thread::scope(|scope| -> Result<usize> {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let sender = sender.clone();
            scope.spawn(move || {
                loop {
                    let task = queue.lock().expect("render queue poisoned").pop_front();
                    let Some((index, track)) = task else {
                        break;
                    };
                    if sender.send(BatchEvent::Started { index }).is_err() {
                        break;
                    }
                    let progress_sender = sender.clone();
                    let result = render_track(
                        &track,
                        config,
                        max_duration,
                        encoder_threads,
                        move |progress| {
                            let _ = progress_sender.send(BatchEvent::Progress { index, progress });
                        },
                    )
                    .map_err(|error| format!("{error:#}"));
                    if sender
                        .send(BatchEvent::Finished {
                            index,
                            outcome: RenderOutcome { track, result },
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let mut failures = 0;
        let mut callback_error = None;
        for event in receiver {
            match event {
                BatchEvent::Started { index } => display.started(index),
                BatchEvent::Progress { index, progress } => display.progress(index, progress),
                BatchEvent::Finished { index, outcome } => {
                    if outcome.result.is_err() {
                        failures += 1;
                    }
                    display.finished(index, &outcome);
                    if callback_error.is_none()
                        && let Err(error) = on_outcome(&outcome)
                    {
                        callback_error = Some(error);
                    }
                }
            }
        }
        display.finish_batch();
        if let Some(error) = callback_error {
            return Err(error).context("failed to update render summary");
        }
        Ok(failures)
    })
}

fn limited_duration(duration: Option<f64>, limit: Option<f64>) -> f64 {
    duration
        .map(|duration| limit.map_or(duration, |limit| duration.min(limit)))
        .unwrap_or_default()
}

#[derive(Clone, Copy)]
struct ActiveProgress {
    media_seconds: f64,
    duration_seconds: f64,
}

struct BatchDisplay {
    quiet: bool,
    terminal: bool,
    started_at: Instant,
    last_draw: Instant,
    line_visible: bool,
    total: usize,
    completed: usize,
    failures: usize,
    titles: Vec<String>,
    expected_durations: Vec<f64>,
    total_media_seconds: f64,
    completed_media_seconds: f64,
    active: BTreeMap<usize, ActiveProgress>,
}

impl BatchDisplay {
    fn new(
        titles: Vec<String>,
        expected_durations: Vec<f64>,
        workers: usize,
        encoder_threads: usize,
        config: &RenderConfig,
        quiet: bool,
    ) -> Self {
        let total_media_seconds = expected_durations.iter().sum();
        if !quiet {
            println!(
                "render plan: {} track(s) | media {} | jobs {} x {} threads | {}x{}@{} | {}",
                titles.len(),
                format_duration(total_media_seconds),
                workers,
                encoder_threads,
                config.width,
                config.height,
                config.fps,
                config.encoder
            );
        }
        Self {
            quiet,
            terminal: io::stdout().is_terminal(),
            started_at: Instant::now(),
            last_draw: Instant::now(),
            line_visible: false,
            total: titles.len(),
            completed: 0,
            failures: 0,
            titles,
            expected_durations,
            total_media_seconds,
            completed_media_seconds: 0.0,
            active: BTreeMap::new(),
        }
    }

    fn started(&mut self, index: usize) {
        self.active.insert(
            index,
            ActiveProgress {
                media_seconds: 0.0,
                duration_seconds: self.expected_durations[index],
            },
        );
        if !self.quiet && !self.terminal {
            println!(
                "[{}/{}] start: {} ({})",
                index + 1,
                self.total,
                self.titles[index],
                known_duration(self.expected_durations[index])
            );
        }
        self.draw(true);
    }

    fn progress(&mut self, index: usize, progress: RenderProgress) {
        let old_duration = self.expected_durations[index];
        if progress.duration_seconds > 0.0
            && (old_duration - progress.duration_seconds).abs() > 0.001
        {
            self.expected_durations[index] = progress.duration_seconds;
            self.total_media_seconds += progress.duration_seconds - old_duration;
        }
        self.active.insert(
            index,
            ActiveProgress {
                media_seconds: progress.media_seconds,
                duration_seconds: progress.duration_seconds,
            },
        );
        self.draw(false);
    }

    fn finished(&mut self, index: usize, outcome: &RenderOutcome) {
        self.clear_line();
        self.active.remove(&index);
        self.completed += 1;
        match &outcome.result {
            Ok(stats) => {
                self.completed_media_seconds += stats.duration_seconds;
                if !self.quiet {
                    println!(
                        "[{}/{}] done: {} | media {} | took {} | {:.2}x | ETA {}",
                        self.completed,
                        self.total,
                        outcome.track.title,
                        format_duration(stats.duration_seconds),
                        format_duration(stats.elapsed_seconds),
                        stats.duration_seconds / stats.elapsed_seconds.max(0.001),
                        self.eta_text()
                    );
                }
            }
            Err(error) => {
                self.failures += 1;
                self.completed_media_seconds += self.expected_durations[index];
                eprintln!(
                    "[{}/{}] failed: {}: {error}",
                    self.completed, self.total, outcome.track.title
                );
            }
        }
        self.draw(true);
    }

    fn finish_batch(&mut self) {
        self.clear_line();
        if self.quiet {
            return;
        }
        let elapsed = self.started_at.elapsed().as_secs_f64();
        println!(
            "finished: {} track(s), {} failed | elapsed {} | media {} | average {:.2}x realtime",
            self.completed,
            self.failures,
            format_duration(elapsed),
            format_duration(self.completed_media_seconds),
            self.completed_media_seconds / elapsed.max(0.001)
        );
    }

    fn draw(&mut self, force: bool) {
        if self.quiet || !self.terminal {
            return;
        }
        if !force && self.last_draw.elapsed().as_millis() < 200 {
            return;
        }
        let elapsed = self.started_at.elapsed().as_secs_f64();
        let processed = self.processed_media_seconds();
        let percent = self.overall_fraction() * 100.0;
        let speed = processed / elapsed.max(0.001);
        let line = format!(
            "[{percent:5.1}% | {}/{}] {} | media {}/{} | elapsed {} | ETA {} | {:.2}x",
            self.completed,
            self.total,
            self.active_text(),
            format_duration(processed),
            known_duration(self.total_media_seconds),
            format_duration(elapsed),
            self.eta_text(),
            speed
        );
        print!("\r\x1b[2K{line}");
        let _ = io::stdout().flush();
        self.line_visible = true;
        self.last_draw = Instant::now();
    }

    fn clear_line(&mut self) {
        if self.terminal && self.line_visible {
            print!("\r\x1b[2K");
            let _ = io::stdout().flush();
            self.line_visible = false;
        }
    }

    fn processed_media_seconds(&self) -> f64 {
        self.completed_media_seconds
            + self
                .active
                .values()
                .map(|progress| progress.media_seconds)
                .sum::<f64>()
    }

    fn overall_fraction(&self) -> f64 {
        if self.total_media_seconds > 0.0 {
            return (self.processed_media_seconds() / self.total_media_seconds).clamp(0.0, 1.0);
        }
        let active_fraction: f64 = self
            .active
            .values()
            .map(|progress| {
                if progress.duration_seconds > 0.0 {
                    progress.media_seconds / progress.duration_seconds
                } else {
                    0.0
                }
            })
            .sum();
        ((self.completed as f64 + active_fraction) / self.total.max(1) as f64).clamp(0.0, 1.0)
    }

    fn eta_text(&self) -> String {
        let elapsed = self.started_at.elapsed().as_secs_f64();
        let processed = self.processed_media_seconds();
        let remaining = (self.total_media_seconds - processed).max(0.0);
        if self.total_media_seconds > 0.0 && remaining <= 0.001 {
            return "0s".to_owned();
        }
        if elapsed < 0.5 || processed <= 0.0 || self.total_media_seconds <= 0.0 {
            return "calculating".to_owned();
        }
        let throughput = processed / elapsed;
        format_duration(remaining / throughput.max(0.001))
    }

    fn active_text(&self) -> String {
        let Some((&index, progress)) = self.active.iter().next() else {
            return "finalizing".to_owned();
        };
        let percent = if progress.duration_seconds > 0.0 {
            progress.media_seconds / progress.duration_seconds * 100.0
        } else {
            0.0
        };
        let title = truncate_title(&self.titles[index], 26);
        let others = self.active.len().saturating_sub(1);
        if others == 0 {
            format!("{title} {percent:.0}%")
        } else {
            format!("{title} {percent:.0}% (+{others})")
        }
    }
}

fn truncate_title(title: &str, max_characters: usize) -> String {
    if title.chars().count() <= max_characters {
        return title.to_owned();
    }
    let mut shortened: String = title
        .chars()
        .take(max_characters.saturating_sub(1))
        .collect();
    shortened.push('…');
    shortened
}

fn known_duration(seconds: f64) -> String {
    if seconds > 0.0 {
        format_duration(seconds)
    } else {
        "unknown".to_owned()
    }
}

pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "--".to_owned();
    }
    if seconds > 0.0 && seconds < 1.0 {
        return format!("{seconds:.1}s");
    }
    let total = seconds.round() as u64;
    let days = total / 86_400;
    let hours = total % 86_400 / 3_600;
    let minutes = total % 3_600 / 60;
    let seconds = total % 60;
    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_human_readable_durations() {
        assert_eq!(format_duration(8.4), "8s");
        assert_eq!(format_duration(0.24), "0.2s");
        assert_eq!(format_duration(125.0), "2m 05s");
        assert_eq!(format_duration(7_325.0), "2h 02m");
    }

    #[test]
    fn truncates_titles_on_character_boundaries() {
        assert_eq!(truncate_title("abcdefgh", 5), "abcd…");
        assert_eq!(truncate_title("测试标题", 3), "测试…");
    }
}
