//! Ctrl-C handling for a program that is driving current through a battery.
//!
//! A signal terminates the process without running `Drop`, so a supply set to
//! 3 A keeps delivering after the program is gone. Every run loop polls this
//! flag and switches the hardware off before returning.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Catch SIGINT, SIGTERM and SIGHUP. Safe to call more than once.
pub fn install() {
    let flag = STOP.get_or_init(|| Arc::new(AtomicBool::new(false)));
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        for sig in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            if let Err(e) = signal_hook::flag::register(sig, flag.clone()) {
                eprintln!("signal {sig}: {e}");
            }
        }
    });
}

/// Whether a stop has been asked for since the last [`clear`].
pub fn requested() -> bool {
    STOP.get()
        .map(|f| f.load(Ordering::Relaxed))
        .unwrap_or(false)
}

pub fn clear() {
    if let Some(f) = STOP.get() {
        f.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signal_raises_the_flag_without_killing_the_process() {
        install();
        clear();
        assert!(!requested());
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        // The handler is asynchronous, so give it a moment to land.
        for _ in 0..50 {
            if requested() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(requested(), "SIGINT did not set the stop flag");
        clear();
        assert!(!requested());
    }
}
