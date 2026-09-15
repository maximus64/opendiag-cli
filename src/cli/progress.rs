use super::style;
use opendiag::events::ProgressUnit;
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const LOG_INTERVAL: Duration = Duration::from_secs(5);
const BAR_WIDTH: usize = 24;
const BAR_FILLED: &str = "\u{2501}";
const BAR_REMAINING: &str = "\u{2500}";
const SEPARATOR: &str = "\u{b7}";

struct Progress {
    done: u64,
    total: u64,
    unit: ProgressUnit,
    started: Instant,
    initial_done: u64,
    last_draw: Option<(u64, Instant)>,
}

pub(super) struct ProgressDisplay<W: Write> {
    output: W,
    terminal: bool,
    color: bool,
    active: Option<Progress>,
    /// Columns occupied by the bar, which is not the byte length of the line:
    /// box-drawing characters and color escapes both distort that.
    drawn_width: usize,
}

impl<W: Write> ProgressDisplay<W> {
    pub(super) fn new(output: W, terminal: bool, color: bool) -> Self {
        Self {
            output,
            terminal,
            color,
            active: None,
            drawn_width: 0,
        }
    }

    pub(super) fn update(
        &mut self,
        done: u64,
        total: u64,
        unit: ProgressUnit,
        now: Instant,
    ) -> io::Result<()> {
        if total == 0 {
            return self.finish();
        }
        let done = done.min(total);
        if self
            .active
            .as_ref()
            .is_some_and(|p| p.total != total || p.unit != unit || done < p.done)
        {
            self.finish()?;
        }
        let inline = self.terminal;
        let interval = if inline {
            REFRESH_INTERVAL
        } else {
            LOG_INTERVAL
        };
        let progress = self.active.get_or_insert(Progress {
            done,
            total,
            unit,
            started: now,
            initial_done: done,
            last_draw: None,
        });
        progress.done = done;
        let due = progress
            .last_draw
            .is_none_or(|(_, last)| now.duration_since(last) >= interval);
        if due || done == total || (inline && self.drawn_width == 0) {
            self.draw(now)?;
        }
        if done == total {
            self.finish()?;
        }
        Ok(())
    }

    pub(super) fn step(&mut self, text: &str) -> io::Result<()> {
        self.finish()?;
        self.message(text)
    }

    pub(super) fn message(&mut self, text: &str) -> io::Result<()> {
        self.clear_line()?;
        writeln!(self.output, "{text}")?;
        self.output.flush()
    }

    pub(super) fn finish(&mut self) -> io::Result<()> {
        if let Some(progress) = &self.active {
            let inline = self.terminal;
            if progress
                .last_draw
                .is_none_or(|(done, _)| done != progress.done)
                || (inline && self.drawn_width == 0)
            {
                self.draw(Instant::now())?;
            }
        }
        self.active = None;
        if self.drawn_width > 0 {
            self.drawn_width = 0;
            writeln!(self.output)?;
            self.output.flush()?;
        }
        Ok(())
    }

    fn clear_line(&mut self) -> io::Result<()> {
        if self.drawn_width > 0 {
            write!(self.output, "\r{:width$}\r", "", width = self.drawn_width)?;
            self.drawn_width = 0;
        }
        Ok(())
    }

    fn draw(&mut self, now: Instant) -> io::Result<()> {
        let Some(progress) = &mut self.active else {
            return Ok(());
        };
        let percent = progress.done as u128 * 100 / progress.total as u128;
        let amount = progress.amount();
        let (speed, eta) = progress.rate(now);
        // `columns` is the printable width, tracked separately so the previous
        // bar can be padded over without counting escapes as columns.
        let (line, columns) = if self.terminal {
            let filled =
                (progress.done as u128 * BAR_WIDTH as u128 / progress.total as u128) as usize;
            let head = format!("{percent:>3}% ");
            let tail = format!(" {amount} {SEPARATOR} {speed} {SEPARATOR} ETA {eta}");
            let columns = head.chars().count() + BAR_WIDTH + tail.chars().count();
            let filled_bar = BAR_FILLED.repeat(filled);
            let remaining_bar = BAR_REMAINING.repeat(BAR_WIDTH - filled);
            let line = if self.color {
                format!(
                    "{bold}{head}{reset}{cyan}{filled_bar}{reset}{dim}{remaining_bar}{tail}{reset}",
                    bold = style::BOLD,
                    cyan = style::CYAN,
                    dim = style::DIM,
                    reset = style::RESET,
                )
            } else {
                format!("{head}{filled_bar}{remaining_bar}{tail}")
            };
            (line, columns)
        } else {
            // Redirected output becomes a log; keep it plain, ASCII and sparse.
            let line = match progress.unit {
                ProgressUnit::Bytes => {
                    format!("Progress: {percent}% ({amount}) | {speed} | ETA {eta}")
                }
                ProgressUnit::Items => format!("Progress: {percent}% ({amount})"),
            };
            let columns = line.chars().count();
            (line, columns)
        };
        progress.last_draw = Some((progress.done, now));
        if self.terminal {
            let padding = self.drawn_width.saturating_sub(columns);
            write!(self.output, "\r{line}{:padding$}", "")?;
            self.drawn_width = self.drawn_width.max(columns);
        } else {
            writeln!(self.output, "{line}")?;
        }
        self.output.flush()
    }
}

impl Progress {
    /// How far along, in the unit the operation counts in.
    fn amount(&self) -> String {
        match self.unit {
            ProgressUnit::Bytes => {
                let (scale, unit) = byte_unit(self.total as f64);
                format!(
                    "{:.1}/{:.1} {unit}",
                    self.done as f64 / scale,
                    self.total as f64 / scale
                )
            }
            ProgressUnit::Items => format!("{}/{}", self.done, self.total),
        }
    }

    /// Rate and remaining time, measured from what has moved since this
    /// display started rather than from the start of the operation.
    fn rate(&self, now: Instant) -> (String, String) {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f64();
        let moved = self.done - self.initial_done;
        if elapsed <= 0.0 || moved == 0 {
            return ("--/s".into(), "--:--".into());
        }
        let rate = moved as f64 / elapsed;
        let remaining = ((self.total - self.done) as f64 / rate).ceil() as u64;
        let speed = match self.unit {
            ProgressUnit::Bytes => {
                let (scale, unit) = byte_unit(rate);
                format!("{:.1} {unit}/s", rate / scale)
            }
            // A DID scan against slow timeouts can run below one per second,
            // where `{:.0}` would report `0/s` beside a multi-hour estimate.
            ProgressUnit::Items if rate < 10.0 => format!("{rate:.1}/s"),
            ProgressUnit::Items => format!("{rate:.0}/s"),
        };
        (speed, duration(remaining))
    }
}

impl<W: Write> Drop for ProgressDisplay<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn byte_unit(bytes: f64) -> (f64, &'static str) {
    let mut scale = 1.0;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if bytes < scale * 1024.0 || unit == "TiB" {
            return (scale, unit);
        }
        scale *= 1024.0;
    }
    unreachable!()
}

fn duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_transfer_updates_in_place_and_keeps_messages_on_separate_lines() {
        let now = Instant::now();
        let mut display = ProgressDisplay::new(Vec::new(), true, false);
        display.update(0, 4096, ProgressUnit::Bytes, now).unwrap();
        display
            .update(
                1024,
                4096,
                ProgressUnit::Bytes,
                now + Duration::from_secs(1),
            )
            .unwrap();
        let text = String::from_utf8(display.output.clone()).unwrap();
        assert!(!text.contains('\n'));
        let bar = format!("{}{}", BAR_FILLED.repeat(6), BAR_REMAINING.repeat(18));
        assert!(
            text.contains(&format!(
                " 25% {bar} 1.0/4.0 KiB {SEPARATOR} 1.0 KiB/s {SEPARATOR} ETA 00:03"
            )),
            "{text}"
        );

        let before = display.output.len();
        display
            .update(
                2048,
                4096,
                ProgressUnit::Bytes,
                now + Duration::from_millis(1050),
            )
            .unwrap();
        assert_eq!(display.output.len(), before, "updates must be throttled");
        display.message("warning: keep the ignition on").unwrap();
        assert!(display
            .output
            .ends_with(b"\rwarning: keep the ignition on\n"));
        display
            .update(
                4096,
                4096,
                ProgressUnit::Bytes,
                now + Duration::from_secs(4),
            )
            .unwrap();
        display.step("Checking programming dependencies").unwrap();
        let text = String::from_utf8(display.output.clone()).unwrap();
        assert_eq!(text.matches("100%").count(), 1);
        assert!(text.ends_with("ETA 00:00\nChecking programming dependencies\n"));
    }

    /// Characters belonging to color escapes, which occupy no columns.
    fn escape_chars(text: &str) -> usize {
        let mut count = 0;
        let mut inside = false;
        for c in text.chars() {
            inside |= c == '\x1b';
            if inside {
                count += 1;
                inside = c != 'm';
            }
        }
        count
    }

    #[test]
    fn colored_bars_repaint_by_column_and_stay_plain_when_color_is_off() {
        let now = Instant::now();
        let mut colored = ProgressDisplay::new(Vec::new(), true, true);
        colored.update(512, 4096, ProgressUnit::Bytes, now).unwrap();
        let text = String::from_utf8(colored.output.clone()).unwrap();
        assert!(text.contains('\x1b'), "{text}");
        assert_eq!(
            colored.drawn_width,
            text.chars().filter(|c| *c != '\r').count() - escape_chars(&text)
        );

        // Without color the bar is still multi-byte, so a byte length would
        // overstate the columns to repaint on the next update.
        let mut plain = ProgressDisplay::new(Vec::new(), true, false);
        plain.update(512, 4096, ProgressUnit::Bytes, now).unwrap();
        let drawn = String::from_utf8(plain.output.clone()).unwrap();
        assert!(!drawn.contains('\x1b'), "{drawn}");
        let line = drawn.trim_start_matches('\r');
        assert_eq!(plain.drawn_width, line.chars().count());
        assert!(plain.drawn_width < line.len(), "{drawn}");
    }

    #[test]
    fn redirected_progress_has_sparse_lines_and_no_terminal_controls() {
        let now = Instant::now();
        let mut display = ProgressDisplay::new(Vec::new(), false, false);
        for i in 0..=50 {
            display
                .update(
                    i * 100,
                    10_240,
                    ProgressUnit::Bytes,
                    now + Duration::from_millis(i * 100),
                )
                .unwrap();
        }
        display
            .update(
                10_240,
                10_240,
                ProgressUnit::Bytes,
                now + Duration::from_secs(6),
            )
            .unwrap();
        let text = String::from_utf8(display.output.clone()).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(!text.contains(['\r', '\x1b']));
        assert!(text.lines().last().unwrap().starts_with("Progress: 100%"));
    }

    #[test]
    fn dropping_an_incomplete_transfer_flushes_its_latest_count_without_claiming_completion() {
        let now = Instant::now();
        let mut output = Vec::new();
        {
            let mut display = ProgressDisplay::new(&mut output, true, false);
            display.update(0, 100, ProgressUnit::Bytes, now).unwrap();
            display
                .update(
                    50,
                    100,
                    ProgressUnit::Bytes,
                    now + Duration::from_millis(50),
                )
                .unwrap();
        }
        let text = String::from_utf8(output).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.contains("50%"));
        assert!(!text.contains("100%"));
    }

    #[test]
    fn counters_keep_their_units_and_percent_never_rounds_up_to_completion() {
        let now = Instant::now();
        let mut display = ProgressDisplay::new(Vec::new(), true, false);
        // An operation with no known length has nothing to draw.
        display.update(1, 0, ProgressUnit::Items, now).unwrap();
        assert!(display.output.is_empty());
        display
            .update(u64::MAX - 1, u64::MAX, ProgressUnit::Bytes, now)
            .unwrap();
        display.step("Scanning").unwrap();
        display.update(1, 2, ProgressUnit::Items, now).unwrap();
        display.update(2, 2, ProgressUnit::Items, now).unwrap();
        let text = String::from_utf8(display.output.clone()).unwrap();
        assert!(text.contains("99%"));
        assert!(!text.contains("NaN") && !text.contains("inf"));
        // Counters are counted, not scaled to byte units.
        assert!(text.contains(" 50% ") && text.ends_with("2/2 \u{b7} --/s \u{b7} ETA --:--\n"));
        assert!(!text.contains("KiB") && !text.contains("/2 B"));
    }

    #[test]
    fn a_did_scan_gets_a_bar_on_a_terminal_and_sparse_counts_once_redirected() {
        let now = Instant::now();
        let scan = |terminal| {
            let mut display = ProgressDisplay::new(Vec::new(), terminal, false);
            for did in 1..=4096u64 {
                display
                    .update(
                        did,
                        4096,
                        ProgressUnit::Items,
                        now + Duration::from_millis(did * 10),
                    )
                    .unwrap();
            }
            String::from_utf8(display.output.clone()).unwrap()
        };

        // On a terminal the scan repaints one line, with a rate and an estimate
        // that a 65536-DID scan needs to be bearable.
        let text = scan(true);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains(&BAR_FILLED.repeat(BAR_WIDTH)), "{text}");
        assert!(text.contains("100/s"), "{text}");
        assert!(
            text.ends_with("100% ") || text.contains("ETA 00:00"),
            "{text}"
        );
        assert!(!text.contains("Progress:"), "{text}");

        // Below ten per second the rate keeps a decimal, so a scan crawling
        // against timeouts does not report `0/s` next to its estimate.
        let mut slow = ProgressDisplay::new(Vec::new(), true, false);
        for did in 1..=4u64 {
            slow.update(
                did,
                4096,
                ProgressUnit::Items,
                now + Duration::from_secs(did * 3),
            )
            .unwrap();
        }
        let text = String::from_utf8(slow.output.clone()).unwrap();
        assert!(text.contains("0.3/s"), "{text}");

        // Redirected, it stays a sparse plain-text log of counts.
        let text = scan(false);
        assert!(!text.contains(['\r', '\x1b']), "{text}");
        assert!(text.lines().count() <= 10, "{text}");
        assert!(
            text.lines().last().unwrap() == "Progress: 100% (4096/4096)",
            "{text}"
        );
    }
}
