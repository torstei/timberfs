//! Informational stderr, suppressible with the global --quiet: notes,
//! progress, summaries — the tool talking to a human. Errors and
//! warnings (something may be WRONG) stay on plain eprintln! and always
//! print.

use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// eprintln! unless --quiet.
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => {
        if !$crate::note::quiet() {
            eprintln!($($arg)*);
        }
    };
}
