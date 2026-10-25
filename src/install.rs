//! `remuda install`: puts `remuda` on PATH as `~/.local/bin/remuda`.
//!
//! herdr only resumes commands named by a plain word on PATH, so mirrors can
//! only come back after a restart once this link exists. The plugin's startup
//! hook runs it on every server start, which also repoints the link after a
//! plugin update moves the binary.

use std::path::{Path, PathBuf};

pub fn run(args: &[String]) -> Result<(), String> {
    let quiet = args.iter().any(|arg| arg == "--quiet");
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .map_err(|err| format!("locate remuda: {err}"))?;
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let dir = PathBuf::from(home).join(".local/bin");
    let link = dir.join("remuda");
    let changed = install_link(&exe, &dir, &link)?;
    if !quiet {
        let verb = if changed { "linked" } else { "already linked" };
        println!("{verb}: {} -> {}", link.display(), exe.display());
        if !dir_on_path(&dir) {
            println!(
                "note: {} is not on PATH; herdr cannot resume mirrors until it is",
                dir.display()
            );
        }
    }
    Ok(())
}

/// Points `link` at `exe`. Returns whether anything changed. Never replaces a
/// file that is not a symlink, since that is not ours.
fn install_link(exe: &Path, dir: &Path, link: &Path) -> Result<bool, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(link).is_ok_and(|target| target == exe) {
                return Ok(false);
            }
            std::fs::remove_file(link)
                .map_err(|err| format!("replace {}: {err}", link.display()))?;
        }
        Ok(_) => {
            return Err(format!(
                "{} exists and is not a link to remuda; leaving it alone",
                link.display()
            ))
        }
        Err(_) => {}
    }
    symlink(exe, link).map_err(|err| format!("link {}: {err}", link.display()))?;
    Ok(true)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("only supported on unix"))
}

fn dir_on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|entry| entry == dir))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("remuda-install-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn links_then_reports_no_change_then_repoints() {
        let dir = scratch("link");
        let (old, new) = (dir.join("old"), dir.join("new"));
        std::fs::write(&old, "").unwrap();
        std::fs::write(&new, "").unwrap();
        let bin = dir.join("bin");
        let link = bin.join("remuda");
        assert!(install_link(&old, &bin, &link).unwrap());
        assert!(!install_link(&old, &bin, &link).unwrap());
        assert!(install_link(&new, &bin, &link).unwrap());
        assert_eq!(std::fs::read_link(&link).unwrap(), new);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn never_replaces_a_real_file() {
        let dir = scratch("file");
        let link = dir.join("remuda");
        std::fs::write(&link, "someone else's").unwrap();
        assert!(install_link(&dir.join("exe"), &dir, &link).is_err());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "someone else's");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
