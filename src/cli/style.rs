//! Terminal presentation: one place for colors, status marks and label widths.
//!
//! Results go to stdout while progress, warnings and details go to stderr, so
//! `auto` colorizes only when both streams are terminals; redirecting either one
//! must never put escape sequences into a file. Marks replace the older
//! `[Ok]`/`[Warn]` tags and stay readable once color is off.

use colored::{ColoredString, Colorize};
use opendiag::preflight::CheckStatus;
use std::{
    fmt::Display,
    io::{self, IsTerminal},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Colorize when stdout and stderr are both terminals.
    #[default]
    Auto,
    /// Colorize even when the output is redirected.
    Always,
    /// Never emit escape sequences.
    Never,
}

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Resolves `--color` once, before anything is printed.
pub(super) fn configure(choice: ColorChoice) {
    let enabled = match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
                && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
                && io::stdout().is_terminal()
                && io::stderr().is_terminal()
        }
    };
    ENABLED.store(enabled, Ordering::Relaxed);
    colored::control::set_override(enabled);
}

pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Raw sequences for output whose printable width must be measured, where a
/// `ColoredString` would hide both the escapes and the global color decision.
pub(super) const BOLD: &str = "\x1b[1m";
pub(super) const CYAN: &str = "\x1b[36m";
pub(super) const DIM: &str = "\x1b[2m";
pub(super) const RESET: &str = "\x1b[0m";

pub(super) const OK: &str = "\u{2714}";
pub(super) const INFO: &str = "\u{2022}";
pub(super) const WARN: &str = "\u{25B2}";
pub(super) const FAIL: &str = "\u{2716}";
pub(super) const STEP: &str = "\u{25B8}";
pub(super) const DETAIL: &str = "\u{00B7}";

/// A finished operation on stdout, or a confirmed condition on stderr.
pub(super) fn ok(text: impl Display) -> String {
    format!("{} {text}", OK.green().bold())
}
pub(super) fn info(text: impl Display) -> String {
    format!("{} {text}", INFO.blue().bold())
}
pub(super) fn warn(text: impl Display) -> String {
    format!("{} {text}", WARN.yellow().bold())
}
pub(super) fn fail(text: impl Display) -> String {
    format!("{} {text}", FAIL.red().bold())
}
/// A phase of a long operation; the progress display clears the bar first.
pub(super) fn step(text: impl Display) -> String {
    format!("{} {}", STEP.cyan().bold(), text.to_string().bold())
}
/// Protocol-level `--verbose` output, kept visually behind everything else.
pub(super) fn detail(text: impl Display) -> String {
    format!("{} {}", DETAIL.dimmed(), text.to_string().dimmed())
}

/// The mark for a preflight check, replacing its debug-formatted name.
pub(super) fn check(status: CheckStatus) -> ColoredString {
    match status {
        CheckStatus::Ok => OK.green(),
        CheckStatus::Info => INFO.blue(),
        CheckStatus::Warn => WARN.yellow(),
        CheckStatus::Fail => FAIL.red(),
    }
    .bold()
}

/// A section heading, such as a module a listing is grouped under.
pub(super) fn heading(text: impl Display) -> ColoredString {
    text.to_string().bold()
}
/// An identifier the user can pass back on the command line.
pub(super) fn key(text: impl Display) -> ColoredString {
    text.to_string().cyan()
}
/// The name of a field whose value follows. Pad before styling: escape
/// sequences count towards a format width but occupy no columns.
pub(super) fn label(text: impl Display) -> ColoredString {
    text.to_string().dimmed()
}
/// A reading, measurement or decoded string.
pub(super) fn value(text: impl Display) -> ColoredString {
    text.to_string().bold()
}
/// A diagnostic trouble code.
pub(super) fn code(text: impl Display) -> ColoredString {
    text.to_string().yellow().bold()
}
/// Supporting text that should not compete with the value beside it.
pub(super) fn muted(text: impl Display) -> ColoredString {
    text.to_string().dimmed()
}
