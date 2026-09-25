//! Status lines, warnings, one progress line. Same shape as cargo.

use std::fmt::Display;
use std::io::{IsTerminal, Write};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
    VeryVerbose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MessageFormat {
    #[default]
    Human,
    /// One-line diagnostics, no JSON.
    Short,
    Json {
        /// `json-render-diagnostics`: human output as well as the JSON.
        render: bool,
        short: bool,
        ansi: bool,
    },
}

impl MessageFormat {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "human" => Self::Human,
            "short" => Self::Short,
            "json" => Self::Json {
                render: false,
                short: false,
                ansi: false,
            },
            "json-diagnostic-short" => Self::Json {
                render: false,
                short: true,
                ansi: false,
            },
            "json-diagnostic-rendered-ansi" => Self::Json {
                render: false,
                short: false,
                ansi: true,
            },
            "json-render-diagnostics" => Self::Json {
                render: true,
                short: false,
                ansi: true,
            },
            other => return Err(format!("unknown --message-format `{other}`")),
        })
    }

    pub fn is_json(self) -> bool {
        matches!(self, Self::Json { .. })
    }

    pub fn render(self) -> bool {
        match self {
            Self::Human | Self::Short => true,
            Self::Json { render, .. } => render,
        }
    }
}

pub struct Shell {
    verbosity: Verbosity,
    color: bool,
    tty: bool,
    format: Mutex<MessageFormat>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    progress_shown: bool,
}

const GREEN: &str = "\x1b[1;32m";
const CYAN: &str = "\x1b[1;36m";
const YELLOW: &str = "\x1b[1;33m";
const RED: &str = "\x1b[1;31m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn color_enabled(flag: Option<&str>, cargo_term: Option<&str>, tty: bool, no_color: bool, term_dumb: bool) -> bool {
    match flag {
        Some("always") => return true,
        Some("never") => return false,
        _ => {}
    }
    match cargo_term {
        Some("always") => true,
        Some("never") => false,
        _ => tty && !no_color && !term_dumb,
    }
}

impl Shell {
    pub fn new(verbose: u8, quiet: bool, color: Option<&str>) -> Self {
        let tty = std::io::stderr().is_terminal();
        let cargo_term = std::env::var("CARGO_TERM_COLOR").ok();
        let color = color_enabled(
            color,
            cargo_term.as_deref(),
            tty,
            std::env::var_os("NO_COLOR").is_some(),
            std::env::var("TERM").ok().is_some_and(|t| t == "dumb"),
        );
        let quiet = quiet || std::env::var("CARGO_TERM_QUIET").ok().is_some_and(|v| v == "true" || v == "1");
        let verbosity = match (quiet, verbose) {
            (true, _) => Verbosity::Quiet,
            (_, 0) => Verbosity::Normal,
            (_, 1) => Verbosity::Verbose,
            _ => Verbosity::VeryVerbose,
        };
        Self {
            verbosity,
            color,
            tty,
            format: Mutex::new(MessageFormat::Human),
            state: Mutex::default(),
        }
    }

    pub fn set_format(&self, format: MessageFormat) {
        *self.format.lock().unwrap() = format;
    }

    pub fn format(&self) -> MessageFormat {
        *self.format.lock().unwrap()
    }

    pub fn json_line(&self, value: &serde_json::Value) {
        println!("{}", serde_json::to_string(value).unwrap_or_default());
    }

    fn show_human(&self) -> bool {
        self.verbosity != Verbosity::Quiet && self.format().render()
    }

    pub fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    pub fn is_verbose(&self) -> bool {
        self.verbosity >= Verbosity::Verbose
    }

    pub fn color(&self) -> bool {
        self.color
    }

    pub fn rustc_json(&self) -> &'static str {
        match self.format() {
            MessageFormat::Short | MessageFormat::Json { short: true, .. } => "diagnostic-short,artifacts,future-incompat",
            MessageFormat::Json { ansi: true, .. } => "diagnostic-rendered-ansi,artifacts,future-incompat",
            MessageFormat::Human if self.color => "diagnostic-rendered-ansi,artifacts,future-incompat",
            _ => "artifacts,future-incompat",
        }
    }

    fn paint(&self, style: &str, s: &str) -> String {
        if self.color { format!("{style}{s}{RESET}") } else { s.to_owned() }
    }

    fn emit(&self, line: &str) {
        let mut st = self.state.lock().unwrap();
        let mut err = std::io::stderr().lock();
        if st.progress_shown {
            let _ = write!(err, "\r\x1b[2K");
            st.progress_shown = false;
        }
        let _ = writeln!(err, "{line}");
    }

    fn verb_line(&self, style: &str, verb: &str, msg: &dyn Display) {
        if !self.show_human() {
            return;
        }
        self.emit(&format!("{} {msg}", self.paint(style, &format!("{verb:>12}"))));
    }

    pub fn status(&self, verb: &str, msg: impl Display) {
        self.verb_line(GREEN, verb, &msg);
    }

    pub fn status_alt(&self, verb: &str, msg: impl Display) {
        self.verb_line(CYAN, verb, &msg);
    }

    pub fn verbose_status(&self, verb: &str, msg: impl Display) {
        if self.is_verbose() {
            self.verb_line(GREEN, verb, &msg);
        }
    }

    pub fn warn(&self, msg: impl Display) {
        if self.show_human() {
            self.emit(&format!("{}: {msg}", self.paint(YELLOW, "warning")));
        }
    }

    pub fn error(&self, msg: impl Display) {
        self.emit(&format!("{}: {msg}", self.paint(RED, "error")));
    }

    pub fn note(&self, msg: impl Display) {
        if self.show_human() {
            self.emit(&format!("{}: {msg}", self.paint(BOLD, "note")));
        }
    }

    pub fn raw(&self, text: &str) {
        let mut st = self.state.lock().unwrap();
        let mut err = std::io::stderr().lock();
        if st.progress_shown {
            let _ = write!(err, "\r\x1b[2K");
            st.progress_shown = false;
        }
        let _ = write!(err, "{text}");
        if !text.ends_with('\n') {
            let _ = writeln!(err);
        }
    }

    pub fn progress(&self, done: usize, total: usize, active: &[String]) {
        if !self.tty || !self.show_human() || total == 0 {
            return;
        }
        let width = 25;
        let filled = done * width / total.max(1);
        let bar = format!("{}>{}", "=".repeat(filled), " ".repeat(width - filled.min(width)));
        let mut names = active.join(", ");
        if names.len() > 60 {
            names.truncate(57);
            names.push_str("...");
        }
        let line = format!(
            "{} [{bar}] {done}/{total}: {names}",
            self.paint(CYAN, &format!("{:>12}", "Building"))
        );
        let mut st = self.state.lock().unwrap();
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{line}");
        let _ = err.flush();
        st.progress_shown = true;
    }

    pub fn clear_progress(&self) {
        let mut st = self.state.lock().unwrap();
        if st.progress_shown {
            let _ = write!(std::io::stderr(), "\r\x1b[2K");
            st.progress_shown = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::color_enabled;

    #[test]
    fn cargo_term_color_matches_cargo() {
        assert!(!color_enabled(None, Some("never"), true, false, false));
        assert!(color_enabled(None, Some("always"), false, true, true));
        assert!(!color_enabled(Some("never"), Some("always"), true, false, false));
        assert!(color_enabled(Some("always"), Some("never"), false, true, true));
        assert!(!color_enabled(None, None, true, true, false));
        assert!(!color_enabled(None, None, false, false, false));
    }
}
