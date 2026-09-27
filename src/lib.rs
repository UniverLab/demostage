//! DemoStage — Demos as Code.
//!
//! Records a terminal session as a sequence of events (a *macro*), normalizes
//! human imperfections into a clean *score* (`demo.toml`), and compiles that
//! score to several output formats. See `SPEC-0002` in the workspace root.

pub mod cli;
pub mod commands;
pub mod error;
pub mod export;
pub mod file_picker;
pub mod fonts;
pub mod model;
pub mod normalize;
pub mod paths;
pub mod validate;

pub use error::{Error, Result};

/// The command a user types inside a capture to end it (see `demo stop`). The
/// normalizer drops it from the score so it never shows up in the finished demo.
pub const STOP_COMMAND: &str = "demo stop";

/// The DemoStage wordmark, shown at the top of a capture. Built with
/// `concat!` (not a `\`-continued literal) so each line keeps its leading
/// spaces — Rust's string-continuation escape would otherwise strip the
/// indentation and collapse the art against the left margin.
pub const BANNER: &str = concat!(
    "
",
    "     █████                                    █████████   █████
",
    "    ░░███                                    ███░░░░░███ ░░███
",
    "  ███████   ██████  █████████████    ██████ ░███    ░░░  ███████    ██████    ███████  ██████
",
    " ███░░███  ███░░███░░███░░███░░███  ███░░███░░█████████ ░░░███░    ░░░░░███  ███░░███ ███░░███
",
    "░███ ░███ ░███████  ░███ ░███ ░███ ░███ ░███ ░░░░░░░░███  ░███      ███████ ░███ ░███░███████
",
    "░███ ░███ ░███░░░   ░███ ░███ ░███ ░███ ░███ ███    ░███  ░███ ███ ███░░███ ░███ ░███░███░░░
",
    "░░████████░░██████  █████░███ █████░░██████ ░░█████████   ░░█████ ░░████████░░███████░░██████
",
    " ░░░░░░░░  ░░░░░░  ░░░░░ ░░░ ░░░░░  ░░░░░░   ░░░░░░░░░     ░░░░░   ░░░░░░░░  ░░░░░███ ░░░░░░
",
    "                                                                              ███ ░███
",
    "                                                                             ░░██████
",
    "                                                                              ░░░░░░",
);

use std::process::ExitCode;

use cli::{Cli, Command};

/// Dispatch a parsed CLI invocation to its command, returning the process exit
/// code. Only `check` and `update` report through the exit code (`update`
/// uses `1` for "an update is available" in `--check` mode); everything else
/// surfaces problems as an [`Error`].
pub fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Capture(args) => commands::capture::run(args).map(|()| ExitCode::SUCCESS),
        Command::Record(args) => commands::record::run(args).map(|()| ExitCode::SUCCESS),
        Command::Export(args) => commands::export::run(args).map(|()| ExitCode::SUCCESS),
        Command::Doctor(args) => commands::doctor::run(args).map(|()| ExitCode::SUCCESS),
        Command::Update(args) => commands::update::run_update(args.check, args.yes)
            .map(|code| ExitCode::from(code as u8)),
        Command::Edit(args) => commands::edit::run(args).map(|()| ExitCode::SUCCESS),
        Command::Stop => commands::stop::run().map(|()| ExitCode::SUCCESS),
        Command::Open(args) => commands::open::run(args).map(|()| ExitCode::SUCCESS),
        Command::Focus(args) => commands::focus::run(args).map(|()| ExitCode::SUCCESS),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        crate::cli::Cli::command().debug_assert();
    }

    #[test]
    fn stop_command_is_correct_string() {
        assert_eq!(crate::STOP_COMMAND, "demo stop");
    }

    #[test]
    fn banner_contains_wordmark() {
        assert!(crate::BANNER.contains("████"));
        assert!(crate::BANNER.contains("██"));
    }

    #[test]
    fn banner_is_non_empty() {
        assert!(!crate::BANNER.is_empty());
        assert!(crate::BANNER.len() > 100);
    }

    #[test]
    fn banner_starts_with_newline() {
        assert!(crate::BANNER.starts_with('\n'));
    }

    #[test]
    fn stop_command_ends_with_stop() {
        assert!(crate::STOP_COMMAND.ends_with("stop"));
    }

    #[test]
    fn stop_command_starts_with_demo() {
        assert!(crate::STOP_COMMAND.starts_with("demo"));
    }

    #[test]
    fn stop_command_has_two_words() {
        let words: Vec<&str> = crate::STOP_COMMAND.split_whitespace().collect();
        assert_eq!(words.len(), 2);
    }

    #[test]
    fn banner_has_multiple_lines() {
        let lines: Vec<&str> = crate::BANNER.lines().collect();
        assert!(lines.len() > 5);
    }

    #[test]
    fn stop_command_no_whitespace_around_words() {
        assert_eq!(crate::STOP_COMMAND, "demo stop");
    }
}
