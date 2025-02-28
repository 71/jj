use std::io;
use std::time::{Duration, Instant};

use crossterm::terminal::{Clear, ClearType};
use jujutsu_lib::git;

use crate::cleanup_guard::CleanupGuard;
use crate::ui::Ui;

pub struct Progress {
    next_print: Instant,
    rate: RateEstimate,
    buffer: String,
    guard: Option<CleanupGuard>,
}

impl Progress {
    pub fn new(now: Instant) -> Self {
        Self {
            next_print: now + INITIAL_DELAY,
            rate: RateEstimate::new(),
            buffer: String::new(),
            guard: None,
        }
    }

    pub fn update(
        &mut self,
        now: Instant,
        progress: &git::Progress,
        ui: &mut Ui,
    ) -> io::Result<()> {
        use std::fmt::Write as _;

        if progress.overall == 1.0 {
            write!(ui, "\r{}", Clear(ClearType::CurrentLine))?;
            return Ok(());
        }

        let rate = progress
            .bytes_downloaded
            .and_then(|x| self.rate.update(now, x));
        if now < self.next_print {
            return Ok(());
        }
        if self.guard.is_none() {
            let guard = ui.output_guard(crossterm::cursor::Show.to_string());
            let guard = CleanupGuard::new(move || {
                drop(guard);
            });
            _ = write!(ui, "{}", crossterm::cursor::Hide);
            self.guard = Some(guard);
        }
        self.next_print = now.min(self.next_print + Duration::from_secs(1) / UPDATE_HZ);

        self.buffer.clear();
        write!(self.buffer, "\r{}", Clear(ClearType::CurrentLine)).unwrap();
        let control_chars = self.buffer.len();
        write!(self.buffer, "{: >3.0}% ", 100.0 * progress.overall).unwrap();
        if let Some(total) = progress.bytes_downloaded {
            let (scaled, prefix) = binary_prefix(total as f32);
            write!(self.buffer, "{scaled: >5.1} {prefix}B ").unwrap();
        }
        if let Some(estimate) = rate {
            let (scaled, prefix) = binary_prefix(estimate);
            write!(self.buffer, "at {scaled: >5.1} {prefix}B/s ").unwrap();
        }

        let bar_width = ui
            .term_width()
            .map(usize::from)
            .unwrap_or(0)
            .saturating_sub(self.buffer.len() - control_chars + 2);
        self.buffer.push('[');
        draw_progress(progress.overall, &mut self.buffer, bar_width);
        self.buffer.push(']');

        write!(ui, "{}", self.buffer)?;
        ui.flush()?;
        Ok(())
    }
}

fn draw_progress(progress: f32, buffer: &mut String, width: usize) {
    const CHARS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    const RESOLUTION: usize = CHARS.len() - 1;
    let ticks = (width as f32 * progress.clamp(0.0, 1.0) * RESOLUTION as f32).round() as usize;
    let whole = ticks / RESOLUTION;
    for _ in 0..whole {
        buffer.push(CHARS[CHARS.len() - 1]);
    }
    if whole < width {
        let fraction = ticks % RESOLUTION;
        buffer.push(CHARS[fraction]);
    }
    for _ in (whole + 1)..width {
        buffer.push(CHARS[0]);
    }
}

const UPDATE_HZ: u32 = 30;
const INITIAL_DELAY: Duration = Duration::from_millis(250);

/// Find the smallest binary prefix with which the whole part of `x` is at most
/// three digits, and return the scaled `x` and that prefix.
fn binary_prefix(x: f32) -> (f32, &'static str) {
    const TABLE: [&str; 9] = ["", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei", "Zi", "Yi"];

    let mut i = 0;
    let mut scaled = x;
    while scaled.abs() >= 1000.0 && i < TABLE.len() - 1 {
        i += 1;
        scaled /= 1024.0;
    }
    (scaled, TABLE[i])
}

struct RateEstimate {
    state: Option<RateEstimateState>,
}

impl RateEstimate {
    fn new() -> Self {
        RateEstimate { state: None }
    }

    /// Compute smoothed rate from an update
    fn update(&mut self, now: Instant, total: u64) -> Option<f32> {
        if let Some(ref mut state) = self.state {
            return Some(state.update(now, total));
        }

        self.state = Some(RateEstimateState {
            total,
            avg_rate: None,
            last_sample: now,
        });
        None
    }
}

struct RateEstimateState {
    total: u64,
    avg_rate: Option<f32>,
    last_sample: Instant,
}

impl RateEstimateState {
    fn update(&mut self, now: Instant, total: u64) -> f32 {
        let delta = total - self.total;
        self.total = total;
        let dt = now - self.last_sample;
        self.last_sample = now;
        let sample = delta as f32 / dt.as_secs_f32();
        match self.avg_rate {
            None => *self.avg_rate.insert(sample),
            Some(ref mut avg_rate) => {
                // From Algorithms for Unevenly Spaced Time Series: Moving
                // Averages and Other Rolling Operators (Andreas Eckner, 2019)
                const TIME_WINDOW: f32 = 2.0;
                let alpha = 1.0 - (-dt.as_secs_f32() / TIME_WINDOW).exp();
                *avg_rate += alpha * (sample - *avg_rate);
                *avg_rate
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bar() {
        let mut buf = String::new();
        draw_progress(0.0, &mut buf, 10);
        assert_eq!(buf, "          ");
        buf.clear();
        draw_progress(1.0, &mut buf, 10);
        assert_eq!(buf, "██████████");
        buf.clear();
        draw_progress(0.5, &mut buf, 10);
        assert_eq!(buf, "█████     ");
        buf.clear();
        draw_progress(0.54, &mut buf, 10);
        assert_eq!(buf, "█████▍    ");
        buf.clear();
    }
}
