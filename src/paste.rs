//! Pasting a clipboard image into an agent on another machine.
//!
//! Terminals do not carry images. Claude Code and similar agents read an
//! image straight from the clipboard of the machine they run on when Ctrl+V
//! is pressed, so in a mirror of a remote agent Ctrl+V finds that machine's
//! clipboard, not this one's. Instead, a remote control mirror catches Ctrl+V:
//! when this machine's clipboard holds an image, it copies it to a temporary
//! file on the source's machine and pastes that file's path, which agents
//! attach as an image the way they do a file dragged into the terminal.
//! Otherwise Ctrl+V goes through as usual.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::herdr::Machine;
use crate::remote::{shell_quote, Remote};

/// Ctrl+V, after modified keys have been turned back into classic ones.
pub const KEY: u8 = 0x16;

/// The clipboard's image as PNG bytes, or `None` when it holds no image.
pub fn clipboard_png() -> Option<Vec<u8>> {
    platform_clipboard_png().filter(|png| png.starts_with(b"\x89PNG"))
}

#[cfg(target_os = "macos")]
fn platform_clipboard_png() -> Option<Vec<u8>> {
    // AppleScript can only write the image to a file, so take it through one.
    let path = std::env::temp_dir().join(format!("remuda-clipboard-{}.png", std::process::id()));
    let script = format!(
        "set f to open for access POSIX file \"{}\" with write permission\n\
         try\n\
         set eof f to 0\n\
         write (the clipboard as «class PNGf») to f\n\
         end try\n\
         close access f",
        path.display()
    );
    let ran = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let png = ran.ok().and_then(|_| std::fs::read(&path).ok());
    let _ = std::fs::remove_file(&path);
    png
}

#[cfg(not(target_os = "macos"))]
fn platform_clipboard_png() -> Option<Vec<u8>> {
    let tools: [&[&str]; 2] = [
        &["wl-paste", "--no-newline", "--type", "image/png"],
        &[
            "xclip",
            "-selection",
            "clipboard",
            "-target",
            "image/png",
            "-out",
        ],
    ];
    tools.iter().find_map(|tool| {
        let output = Command::new(tool[0])
            .args(&tool[1..])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
    })
}

/// Copies `png` to a new file in a private `remuda-<uid>` folder of the
/// machine's temporary directory and returns its path there. Pasted images
/// more than a day old are removed on the way.
///
/// The temporary directory may be a shared `/tmp`, so the folder must be a
/// real directory owned by this user, is kept to mode 700, and the image is
/// written with umask 077 and noclobber: nobody else can read it, and an
/// existing file or symlink in its place is never written through.
pub fn upload(machine: &Machine, png: &[u8]) -> Result<String, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let script = upload_script(stamp);
    let mut child = Remote::new(machine)
        // Through `sh`, whatever the login shell there.
        .shell_command(&format!("exec sh -c {}", shell_quote(&script)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not run ssh: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(png)
            .map_err(|err| format!("could not send the image: {err}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("could not send the image: {err}"))?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || path.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("could not send the image: {}", stderr.trim()));
    }
    // It is typed into the agent's prompt: no escapes or line breaks.
    if path.chars().any(char::is_control) {
        return Err("could not send the image: unexpected path".into());
    }
    Ok(path)
}

fn upload_script(stamp: u128) -> String {
    format!(
        "umask 077; set -C; \
         d=\"${{TMPDIR:-/tmp}}\"; d=\"${{d%/}}/remuda-$(id -u)\"; \
         mkdir -m 700 \"$d\" 2>/dev/null; \
         if [ -L \"$d\" ] || [ ! -d \"$d\" ] || [ ! -O \"$d\" ]; then \
         echo \"$d is not a private directory\" >&2; exit 1; fi; \
         chmod 700 \"$d\" || exit 1; \
         find \"$d\" -maxdepth 1 -type f -name 'paste-*.png' -mtime +1 -delete 2>/dev/null; \
         f=\"$d/paste-{stamp}.png\"; cat > \"$f\" && printf '%s' \"$f\""
    )
}

/// `path` as a bracketed paste, the way a terminal pastes a dropped file.
pub fn as_paste(path: &str) -> Vec<u8> {
    [b"\x1b[200~", path.as_bytes(), b"\x1b[201~"].concat()
}

/// Splits keys around each Ctrl+V: the keys before it and whether a Ctrl+V
/// ends that run.
pub fn split(keys: &[u8]) -> Vec<(&[u8], bool)> {
    let mut runs = Vec::new();
    let mut rest = keys;
    while let Some(at) = rest.iter().position(|&byte| byte == KEY) {
        runs.push((&rest[..at], true));
        rest = &rest[at + 1..];
    }
    if !rest.is_empty() {
        runs.push((rest, false));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_split_around_each_ctrl_v() {
        assert_eq!(split(b"ab"), [(&b"ab"[..], false)]);
        assert_eq!(
            split(b"a\x16\x16b"),
            [(&b"a"[..], true), (&b""[..], true), (&b"b"[..], false)]
        );
        assert_eq!(split(b"\x16"), [(&b""[..], true)]);
    }

    /// Runs the upload script here under `sh`, with `tmp` as `$TMPDIR`.
    #[cfg(unix)]
    fn run_upload(tmp: &std::path::Path, stamp: u128) -> std::process::Output {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(upload_script(stamp))
            .env("TMPDIR", tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"png").unwrap();
        child.wait_with_output().unwrap()
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remuda-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn an_upload_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = scratch("private");
        let output = run_upload(&tmp, 1);
        assert!(output.status.success(), "{output:?}");
        let path = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"png");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn an_upload_never_writes_through_a_symlink() {
        let tmp = scratch("symlink");
        let elsewhere = tmp.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
        // The folder itself is a link: refused.
        std::os::unix::fs::symlink(&elsewhere, tmp.join(format!("remuda-{}", uid.trim()))).unwrap();
        assert!(!run_upload(&tmp, 2).status.success());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        // A link where the image goes: not followed.
        std::fs::remove_file(tmp.join(format!("remuda-{}", uid.trim()))).unwrap();
        assert!(run_upload(&tmp, 3).status.success());
        let target = elsewhere.join("victim");
        std::fs::write(&target, b"keep").unwrap();
        let dir = tmp.join(format!("remuda-{}", uid.trim()));
        std::os::unix::fs::symlink(&target, dir.join("paste-4.png")).unwrap();
        assert!(!run_upload(&tmp, 4).status.success());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn old_images_are_pruned_and_nothing_else() {
        let tmp = scratch("prune");
        let first = run_upload(&tmp, 5);
        assert!(first.status.success(), "{first:?}");
        let old = std::path::PathBuf::from(String::from_utf8(first.stdout).unwrap());
        let dir = old.parent().unwrap().to_path_buf();
        let victim = tmp.join("victim.png");
        std::fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("paste-link.png")).unwrap();
        std::fs::create_dir(dir.join("paste-dir.png")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
        for path in [&old, &victim] {
            let aged = Command::new("touch")
                .args(["-t", "202001010000"])
                .arg(path)
                .status();
            assert!(aged.unwrap().success());
        }
        assert!(run_upload(&tmp, 6).status.success());
        assert!(!old.exists(), "a day-old image is removed");
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(dir.join("paste-dir.png").is_dir());
        assert!(dir.join("notes.txt").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_path_pastes_bracketed() {
        assert_eq!(as_paste("/tmp/a.png"), b"\x1b[200~/tmp/a.png\x1b[201~");
    }
}
