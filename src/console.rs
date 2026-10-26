//! Terminal and log output for the command.

use std::{
    io::IsTerminal,
    sync::atomic::{AtomicBool, Ordering},
};

use anstyle::{AnsiColor, Style};
use time::OffsetDateTime;
use vault_mirror::mirror::Reporter;

/// Writes progress for people at a terminal, in the style of Cargo: a
/// right-aligned verb, then the rest of the sentence. Elsewhere, such as in
/// container logs, each line is a timestamped log line instead.
#[derive(Default)]
pub struct Console {
    warned: AtomicBool,
}

const VERB: Style = AnsiColor::Green.on_default().bold();
const PROGRESS: Style = AnsiColor::Cyan.on_default().bold();
const WARNING: Style = AnsiColor::Yellow.on_default().bold();
pub const ERROR: Style = AnsiColor::Red.on_default().bold();
const NOTE: Style = AnsiColor::Cyan.on_default().bold();

/// Whether a progress bar occupies the terminal's last line.
static PROGRESS_SHOWN: AtomicBool = AtomicBool::new(false);

impl Console {
    pub fn has_warnings(&self) -> bool {
        self.warned.load(Ordering::Relaxed)
    }

    fn terminal() -> bool {
        std::io::stdout().is_terminal()
    }

    /// Removes the progress bar so that a line can take its place.
    pub fn clear_progress() {
        if PROGRESS_SHOWN.swap(false, Ordering::Relaxed) {
            // Written directly: anstream would strip the control sequence
            // along with colours when NO_COLOR is set.
            eprint!("\r\x1b[K");
        }
    }

    pub fn status(verb: &str, detail: &str) {
        if !Self::terminal() {
            return Self::log("INFO", &format!("{verb} {detail}"));
        }
        Self::clear_progress();
        anstream::println!("{VERB}{verb:>12}{VERB:#} {detail}");
    }

    pub fn warning(message: &str) {
        if !Self::terminal() {
            return Self::log("WARN", message);
        }
        Self::clear_progress();
        anstream::eprintln!("{WARNING}warning:{WARNING:#} {message}");
    }

    /// The last line of a command. Its verb turns yellow when the command
    /// printed warnings, which are on the lines above.
    pub fn finished(detail: &str, warned: bool) {
        if !Self::terminal() {
            return Self::log("INFO", &format!("Finished {detail}"));
        }
        Self::clear_progress();
        let style = if warned { WARNING } else { VERB };
        anstream::println!("{style}{:>12}{style:#} {detail}", "Finished");
    }

    pub fn note(message: &str) {
        if !Self::terminal() {
            return Self::log("INFO", message);
        }
        Self::clear_progress();
        anstream::println!("{NOTE}note:{NOTE:#} {message}");
    }

    pub fn error(message: &str) {
        if !Self::terminal() {
            return Self::log("ERROR", message);
        }
        Self::clear_progress();
        anstream::eprintln!("{ERROR}error:{ERROR:#} {message}");
    }

    fn log(level: &str, message: &str) {
        let now = OffsetDateTime::now_utc();
        let line = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z {level} {message}",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );
        match level {
            "WARN" | "ERROR" => eprintln!("{line}"),
            _ => println!("{line}"),
        }
    }
}

impl Reporter for Console {
    fn status(&self, verb: &str, detail: &str) {
        Self::status(verb, detail);
    }
    fn warn(&self, message: &str) {
        self.warned.store(true, Ordering::Relaxed);
        Self::warning(message);
    }
    fn progress(&self, verb: &str, done: usize, total: usize, unit: &str) {
        const WIDTH: usize = 25;
        if !Self::terminal() || !std::io::stderr().is_terminal() {
            return;
        }
        if done >= total {
            return Self::clear_progress();
        }
        let filled = WIDTH * done / total;
        let bar = format!("{}>{}", "=".repeat(filled), " ".repeat(WIDTH - filled - 1));
        eprint!("\r\x1b[K");
        anstream::eprint!("{PROGRESS}{verb:>12}{PROGRESS:#} [{bar}] {done}/{total} {unit}");
        PROGRESS_SHOWN.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporting_warnings_belong_to_one_run() {
        let reporter = Console::default();
        let next_run = Console::default();
        assert!(!reporter.has_warnings());
        reporter.warn("a notification could not be sent");
        assert!(reporter.has_warnings());
        assert!(!next_run.has_warnings());
    }
}
