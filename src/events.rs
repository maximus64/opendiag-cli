use crate::{preflight::Preflight, DiagError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

/// Units used by an operation's progress counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressUnit {
    Items,
    Bytes,
}

pub enum Event<'a> {
    Step(&'a str),
    /// A zero total represents an operation with no known length.
    Progress {
        done: u64,
        total: u64,
        unit: ProgressUnit,
    },
    Warning(&'a str),
    Detail(&'a str),
}
pub trait Events {
    fn emit(&mut self, event: Event<'_>);
}
pub struct NoEvents;
impl Events for NoEvents {
    fn emit(&mut self, _: Event<'_>) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Continue,
    Abort,
}
pub trait Interaction {
    /// Present a preflight, including failed checks, without authorizing anything.
    fn report(&mut self, _: &Preflight) {}
    fn confirm(&mut self, preflight: &Preflight) -> bool;
    fn prompt(&mut self, text: &str) -> Answer;
}

/// Thread-safe request; the session decides at which boundaries it is safe to stop.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);
impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub fn check(&self) -> Result<(), DiagError> {
        if self.is_cancelled() {
            Err(DiagError::Cancelled)
        } else {
            Ok(())
        }
    }
    pub fn wait(&self, duration: Duration) {
        let deadline = std::time::Instant::now() + duration;
        while !self.is_cancelled() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(25)));
        }
    }
}
