//! Terminal and log output for the command.

use std::{
    io::IsTerminal,
    sync::atomic::{AtomicBool, Ordering},
};

use anstyle::{AnsiColor, Style};
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr;
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
const ERROR: Style = AnsiColor::Red.on_default().bold();
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
        let style = status_style(verb);
        let detail = prose(detail, 13);
        anstream::println!("{style}{verb:>12}{style:#} {detail}");
    }

    pub fn warning(message: &str) {
        if !Self::terminal() {
            return Self::log("WARN", message);
        }
        Self::clear_progress();
        let message = prose(message, 9);
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
        let detail = prose(detail, 13);
        anstream::println!("{style}{:>12}{style:#} {detail}", "Finished");
    }

    pub fn note(message: &str) {
        if !Self::terminal() {
            return Self::log("INFO", message);
        }
        Self::clear_progress();
        let message = prose(message, 6);
        anstream::println!("{NOTE}note:{NOTE:#} {message}");
    }

    pub fn error(message: &str) {
        if !Self::terminal() {
            return Self::log("ERROR", message);
        }
        Self::command_error(message);
    }

    /// Top-level command errors stay plain in redirected output.
    pub fn command_error(message: &str) {
        Self::clear_progress();
        let message = if std::io::stderr().is_terminal() {
            prose(message, 7)
        } else {
            message.to_owned()
        };
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
        if !Self::terminal() || !std::io::stderr().is_terminal() {
            return;
        }
        if done >= total {
            return Self::clear_progress();
        }
        eprint!("\r\x1b[K");
        anstream::eprint!(
            "{PROGRESS}{verb:>12}{PROGRESS:#} {}",
            progress_bar(done, total, unit)
        );
        PROGRESS_SHOWN.store(true, Ordering::Relaxed);
    }
}

fn progress_bar(done: usize, total: usize, unit: &str) -> String {
    const WIDTH: usize = 25;
    let filled = WIDTH * done / total;
    let bar = format!("{}>{}", "=".repeat(filled), " ".repeat(WIDTH - filled - 1));
    let digits = total.to_string().len();
    format!("[{bar}] {done:>digits$}/{total} {unit}")
}

fn status_style(verb: &str) -> Style {
    match verb {
        "Backing up" | "Waiting" | "Stopping" => PROGRESS,
        _ => VERB,
    }
}

fn prose(message: &str, indent: usize) -> String {
    let width = terminal_size::terminal_size()
        .map_or(80, |(terminal_size::Width(width), _)| usize::from(width));
    terminal_text(message, indent, width)
}

fn terminal_text(message: &str, indent: usize, width: usize) -> String {
    let padding = " ".repeat(indent);
    let mut text = String::new();
    let mut column = indent;
    for (index, line) in message.split('\n').enumerate() {
        if index > 0 {
            text.push('\n');
            text.push_str(&padding);
            column = indent;
        }
        for word in line.split_inclusive(' ') {
            if column > indent && column + word.trim_end_matches(' ').width() > width {
                text.truncate(text.trim_end_matches(' ').len());
                text.push('\n');
                text.push_str(&padding);
                column = indent;
            }
            text.push_str(word);
            column += word.width();
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_counts_keep_the_total_and_unit_in_place() {
        let first = progress_bar(0, 1250, "attachments");
        for done in [9, 10, 99, 100, 999, 1000, 1249] {
            let line = progress_bar(done, 1250, "attachments");
            assert_eq!(line.find('/'), first.find('/'), "{line}");
            assert_eq!(line.find("attachments"), first.find("attachments"));
        }
    }

    #[test]
    fn reporting_warnings_belong_to_one_run() {
        let reporter = Console::default();
        let next_run = Console::default();
        assert!(!reporter.has_warnings());
        reporter.warn("a notification could not be sent");
        assert!(reporter.has_warnings());
        assert!(!next_run.has_warnings());
    }

    #[test]
    fn terminal_prose_wraps_without_breaking_words() {
        let message =
            "Could not send the notification: the server answered 503 Service Unavailable";
        let text = terminal_text(message, 9, 80);
        assert_eq!(
            text,
            "Could not send the notification: the server answered 503 Service\n         Unavailable"
        );
        assert_eq!(
            terminal_text("export to /two  spaces/秘密", 9, 80),
            "export to /two  spaces/秘密"
        );
        assert_eq!(terminal_text("秘密 data", 9, 16), "秘密\n         data");
        assert_eq!(
            terminal_text("an-indivisible-long-path", 9, 16),
            "an-indivisible-long-path"
        );
    }

    #[test]
    fn active_steps_use_the_progress_colour() {
        for verb in ["Backing up", "Waiting", "Stopping"] {
            assert_eq!(status_style(verb), PROGRESS);
        }
        for verb in ["Backed up", "Saved", "Verified", "Created"] {
            assert_eq!(status_style(verb), VERB);
        }
    }
}
