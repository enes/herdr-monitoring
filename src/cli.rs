pub const HELP: &str =
    "Usage: herdr-resource-monitor <summary|focused|open-summary|toggle-focused>\n\n\
    summary   Summarize recognized Herdr agents\n\
    focused   Follow the focused normal Herdr pane\n\
    open-summary    Open the summary popup (Herdr plugin action)\n\
    toggle-focused Open or close the right monitor (Herdr plugin action)\n\n\
    Summary: Up/Down select, s sorts CPU/Memory/Name, d opens details or returns.\n\
    Focused: d toggles technical details. Arrow keys scroll detail views.\n\n\
    Close either view with q, Escape, or Ctrl-C.\n\
    --help    Show this help\n\
    --version Show the version";

use herdr_resource_monitor::actions::Action;
pub use herdr_resource_monitor::app::Mode;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Mode),
    Action(Action),
    Help,
    Version,
}

pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let first = args.next().ok_or("expected a mode or action")?;
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument: {extra}"));
    }
    match first.as_str() {
        "summary" => Ok(Command::Run(Mode::Summary)),
        "focused" => Ok(Command::Run(Mode::Focused)),
        "open-summary" => Ok(Command::Action(Action::OpenSummary)),
        "toggle-focused" => Ok(Command::Action(Action::ToggleFocused)),
        "--help" | "-h" => Ok(Command::Help),
        "--version" | "-V" => Ok(Command::Version),
        _ => Err(format!("unknown command: {first}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_both_modes_and_help_without_starting_a_terminal() {
        for (arg, expected) in [
            ("summary", Command::Run(Mode::Summary)),
            ("focused", Command::Run(Mode::Focused)),
            ("open-summary", Command::Action(Action::OpenSummary)),
            ("toggle-focused", Command::Action(Action::ToggleFocused)),
            ("--help", Command::Help),
            ("--version", Command::Version),
        ] {
            assert_eq!(parse([arg.to_string()].into_iter()), Ok(expected));
        }
    }

    #[test]
    fn rejects_missing_unknown_and_extra_arguments() {
        for args in [vec![], vec!["other"], vec!["summary", "focused"]] {
            assert!(parse(args.into_iter().map(str::to_string)).is_err());
        }
    }
}
