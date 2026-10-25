//! remuda: pick panes from any machine into a herdr view of your own.
//!
//! Subcommands: `view` (a mirror pane), `pick` (the picker popup) and
//! `open-picker` (the plugin action that opens the popup).

mod geometry;
mod herdr;
mod pick;
mod view;

use std::process::ExitCode;

fn main() -> ExitCode {
    let command = std::env::args().nth(1).unwrap_or_else(|| "view".into());
    let result = match command.as_str() {
        "view" => view::run(),
        "pick" => pick::run(),
        "open-picker" => herdr::json(
            &[
                "plugin",
                "pane",
                "open",
                "--plugin",
                &herdr::plugin_id(),
                "--entrypoint",
                "pick",
            ],
            None,
        )
        .map(|_| ()),
        other => Err(format!("unknown command {other:?}")),
    };
    let Err(err) = result else {
        return ExitCode::SUCCESS;
    };
    eprintln!("{} remuda: {err}", herdr::MARK);
    if command == "view" {
        // The pane closes when this process exits; keep the error readable.
        eprintln!("press enter to close");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    ExitCode::FAILURE
}
