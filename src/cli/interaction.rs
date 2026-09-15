use super::progress::ProgressDisplay;
use super::style;
use opendiag::{
    events::{Answer, Event, Events, Interaction},
    preflight::Preflight,
};
use std::{
    cell::RefCell,
    io::{self, IsTerminal, Write},
    rc::Rc,
    time::Instant,
};

#[derive(Clone)]
pub struct TerminalEvents {
    verbose: bool,
    progress: Rc<RefCell<ProgressDisplay<io::Stderr>>>,
}
impl TerminalEvents {
    pub fn new(verbose: bool) -> Self {
        let output = io::stderr();
        let terminal =
            output.is_terminal() && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
        Self {
            verbose,
            progress: Rc::new(RefCell::new(ProgressDisplay::new(
                output,
                terminal,
                terminal && style::enabled(),
            ))),
        }
    }

    pub fn finish(&self) {
        let _ = self.progress.borrow_mut().finish();
    }
}
impl Events for TerminalEvents {
    fn emit(&mut self, event: Event<'_>) {
        let mut progress = self.progress.borrow_mut();
        // Losing terminal output must not interrupt an ECU programming operation.
        let _ = match event {
            Event::Step(text) => progress.step(&style::step(text)),
            Event::Warning(text) => progress.message(&style::warn(text)),
            Event::Detail(text) if self.verbose => progress.message(&style::detail(text)),
            Event::Progress { done, total, unit } => {
                progress.update(done, total, unit, Instant::now())
            }
            _ => Ok(()),
        };
    }
}
pub struct TerminalInteraction {
    pub yes: bool,
}
/// Each check is marked rather than tagged with its status name, so the column
/// of values stays aligned whatever the outcome.
fn write_preflight(out: &mut impl Write, preflight: &Preflight) -> io::Result<()> {
    writeln!(
        out,
        "{} {}",
        style::muted("Preflight"),
        style::heading(&preflight.operation)
    )?;
    for check in &preflight.checks {
        writeln!(
            out,
            "  {} {} {}",
            style::check(check.status),
            style::label(format!("{:<18}", check.name)),
            style::value(&check.value)
        )?;
    }
    writeln!(out)?;
    writeln!(out, "{}", style::warn(&preflight.risk))
}
pub fn show_preflight(preflight: &Preflight) {
    // A terminal that has gone away must not abort a programming operation.
    let _ = write_preflight(&mut io::stderr(), preflight);
}
fn read_answer(prompt: &str) -> bool {
    eprint!("{prompt}");
    if io::stderr().flush().is_err() {
        return false;
    }
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
/// The question every destructive operation stops on.
fn confirmation() -> String {
    format!("{} {} ", style::heading("Continue?"), style::muted("[y/N]"))
}
impl Interaction for TerminalInteraction {
    fn report(&mut self, preflight: &Preflight) {
        show_preflight(preflight);
    }
    fn confirm(&mut self, _: &Preflight) -> bool {
        self.yes || read_answer(&confirmation())
    }
    fn prompt(&mut self, text: &str) -> Answer {
        if read_answer(&format!("{}\n{}", style::warn(text), confirmation())) {
            Answer::Continue
        } else {
            Answer::Abort
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendiag::preflight::CheckStatus;

    #[test]
    fn preflight_checks_are_marked_and_keep_their_values_in_one_column() {
        let mut preflight = Preflight::new("Program engine firmware", "Interrupting it bricks it");
        preflight.add("battery", "12.4 V", CheckStatus::Ok);
        preflight.add("calibration", "37805-RZA-A560", CheckStatus::Info);
        preflight.add("tested", "unverified strategy", CheckStatus::Warn);
        preflight.add(
            "compatibility",
            "image targets another ECU",
            CheckStatus::Fail,
        );
        let mut out = Vec::new();
        write_preflight(&mut out, &preflight).unwrap();
        let text = String::from_utf8(out).unwrap();

        // The status names themselves must not reappear as text.
        for tag in ["[Ok]", "[Info]", "[Warn]", "[Fail]", "Ok", "Fail"] {
            assert!(!text.contains(tag), "{tag} in {text}");
        }
        for (mark, expected) in [
            (style::OK, 1),
            (style::INFO, 1),
            // Once for the check, once for the risk the operation carries.
            (style::WARN, 2),
            (style::FAIL, 1),
        ] {
            assert_eq!(text.matches(mark).count(), expected, "{mark} in {text}");
        }
        let columns: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("  "))
            .map(|line| {
                line.find("12.4 V")
                    .or(line.find("37805"))
                    .or(line.find("unverified"))
                    .or(line.find("image"))
            })
            .collect();
        assert_eq!(columns.len(), 4);
        assert!(columns.iter().all(|column| *column == columns[0]), "{text}");
        assert!(text.ends_with("Interrupting it bricks it\n"), "{text}");
    }
}
