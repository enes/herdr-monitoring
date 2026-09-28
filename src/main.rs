mod cli;
mod ui;

use herdr_resource_monitor::app::{LaunchContext, MonitorWorker};
use std::process::ExitCode;

fn main() -> ExitCode {
    let command = match cli::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("herdr-resource-monitor: {message}\n\n{}", cli::HELP);
            return ExitCode::from(2);
        }
    };

    match command {
        cli::Command::Help => println!("{}", cli::HELP),
        cli::Command::Version => println!("herdr-resource-monitor {}", env!("CARGO_PKG_VERSION")),
        cli::Command::Action(action) => match herdr_resource_monitor::actions::run(action) {
            Ok(message) => println!("{message}"),
            Err(error) => {
                eprintln!("herdr-resource-monitor: {error}");
                return ExitCode::FAILURE;
            }
        },
        cli::Command::Run(mode) => {
            let context = match LaunchContext::from_env() {
                Ok(context) => context,
                Err(error) => {
                    eprintln!("herdr-resource-monitor: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let worker = match MonitorWorker::start(mode, context) {
                Ok(worker) => worker,
                Err(error) => {
                    eprintln!("herdr-resource-monitor: {error}");
                    return ExitCode::FAILURE;
                }
            };
            if let Err(error) = ui::run(mode, worker) {
                eprintln!("herdr-resource-monitor: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
