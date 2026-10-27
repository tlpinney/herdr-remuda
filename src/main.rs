//! remuda: pick panes from any machine into a herdr view of your own.
//!
//! Subcommands:
//!   view [options]           a mirror pane (options or REMUDA_* variables)
//!   herd [command] ...       named herds of remote agents, and their board
//!   send AGENT[@MACHINE] ... a message typed into an agent once it is idle
//!   pick                     the picker popup
//!   save                     the "Save view" popup
//!   open-popup <entrypoint>  open one of the plugin's popups (plugin actions)
//!   install [--quiet]        put remuda on PATH so herdr can resume mirrors
//!   views list|open <name>|delete <name>
//!   --skill                  print the agent skill file

mod config;
mod context;
mod geometry;
mod github;
mod herd;
mod herdr;
mod herds;
mod input;
mod install;
mod pick;
mod presence;
mod remote;
mod report;
mod save;
mod send;
mod spec;
mod view;
mod views;

use std::process::ExitCode;

/// The agent skill, built in so `remuda --skill` matches this release, as
/// `herdr --skill` does.
const SKILL: &str = include_str!("../skills/remuda/SKILL.md");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("view");
    let rest = args.get(1..).unwrap_or_default();
    let result = match command {
        "view" => view::run(rest),
        "herd" => herd::run(rest),
        "send" => send::run(rest),
        "pick" => pick::run(),
        "save" => save::run(),
        "open-popup" => open_popup(rest),
        "install" => install::run(rest),
        "views" => views_command(rest),
        "--skill" => {
            print!("{SKILL}");
            Ok(())
        }
        other => Err(format!("unknown command {other:?}")),
    };
    let Err(err) = result else {
        return ExitCode::SUCCESS;
    };
    eprintln!("{} remuda: {err}", herdr::MARK);
    if matches!(command, "view" | "pick" | "save") {
        // The pane closes when this process exits; keep the error readable.
        eprintln!("press enter to close");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    ExitCode::FAILURE
}

fn open_popup(args: &[String]) -> Result<(), String> {
    let entrypoint = args.first().ok_or("open-popup needs an entrypoint")?;
    let plugin = herdr::plugin_id();
    herdr::json(
        &[
            "plugin",
            "pane",
            "open",
            "--plugin",
            &plugin,
            "--entrypoint",
            entrypoint,
        ],
        None,
    )
    .map(|_| ())
}

fn views_command(args: &[String]) -> Result<(), String> {
    match args {
        [] => Err(
            "usage: remuda views list | save <name> <tab_id> | open <name> [--workspace ID] [--no-focus] | delete <name>"
                .into(),
        ),
        [list] if list == "list" => {
            for view in views::list() {
                println!(
                    "{}\t{} panes\t{} mirrors",
                    view.name,
                    view.pane_count(),
                    view.mirror_count()
                );
            }
            Ok(())
        }
        [save, name, tab_id] if save == "save" => {
            let view = views::save_tab(name, tab_id)?;
            println!(
                "saved {:?}: {} panes, {} mirrors",
                view.name,
                view.pane_count(),
                view.mirror_count()
            );
            Ok(())
        }
        [open, name, options @ ..] if open == "open" => {
            let mut workspace = std::env::var("HERDR_WORKSPACE_ID").ok();
            let mut focus = true;
            let mut options = options.iter();
            while let Some(option) = options.next() {
                match option.as_str() {
                    "--workspace" => workspace = options.next().cloned(),
                    "--no-focus" => focus = false,
                    other => return Err(format!("unknown views open option {other}")),
                }
            }
            let workspace = workspace.ok_or("no workspace: pass --workspace or run inside herdr")?;
            views::open(&views::load(name)?, &workspace, focus)
        }
        [delete, name] if delete == "delete" => views::delete(name),
        _ => Err(format!("unknown views command {:?}", args.join(" "))),
    }
}
