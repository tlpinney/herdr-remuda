# remuda

A [Herdr](https://herdr.dev) plugin for picking the panes you want to work with, from this machine or any saved SSH machine, and mirroring them into a view of your own. (A *remuda* is the herd of horses a rider picks the day's mounts from.)

- **Mirrors.** A mirror pane shows another pane live, local or remote. Watch it read-only, or drive it: type, scroll, click.
- **Resize policies.** When you drive a mirror you choose who sets the source's size: `follow` (your mirror), `source` (its own layout), `smallest` of the two, or `freeze` at the size it had.
- **Saved views.** Arrange mirrors in a tab and save it under a name; reopen it later as a new tab with the same layout.
- **Survives restarts and drops.** Herdr reopens mirrors after a server restart, and a mirror reconnects by itself after a network drop, sleep, or a remote restart.
- **Herds.** Start several agents on a remote machine and keep working; each one opens beside you when it needs input, and goes away once you answer.
- **Shows who is watching.** The source machine's sidebar marks a mirrored agent, e.g. `claude ⇄ travis-air (control)`, and your sidebar lists the mirror with the source's state.

It uses only Herdr's CLI and socket API (`terminal session observe` and `control`), so it needs no changes to Herdr, and nothing has to be installed on the machines you mirror from.

## Requirements

- Herdr 0.9.2 or later, on this machine and on the machines you mirror from
- macOS (Apple Silicon or Intel) or Linux (x86_64 or ARM64)
- `curl` or `wget`, and `ssh-keygen` (OpenSSH), to download and verify the prebuilt binary
- For remote panes: a saved Herdr machine (`herdr machine add`) reachable with non-interactive SSH

## Install

```bash
herdr plugin install tlpinney/remuda
```

Install downloads the prebuilt binary for your platform from the matching GitHub Release. It installs it only if the release's `SHA256SUMS` carries a valid signature from a key in [`signing/allowed_signers`](signing/allowed_signers) and the binary matches its checksum there; otherwise the install stops. If no release exists for that version, or `ssh-keygen` is missing, it builds from source instead, which needs a Rust toolchain (`cargo`).

The plugin's startup hook links `~/.local/bin/remuda`, because Herdr only resumes commands it finds on `PATH`. Run `remuda install` once yourself to set it up before the next server start. It never replaces a file it did not create.

Bind the two actions in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "remuda.pick"
description = "remuda: mirror a pane or open a view"

[[keys.command]]
key = "prefix+shift+m"
type = "plugin_action"
command = "remuda.save-view"
description = "remuda: save this tab as a view"
```

For local development, build and link the working tree instead:

```bash
cargo build --release
herdr plugin link .
```

## Use

**Pick a pane** (`prefix+m`). The picker lists every pane on this machine and on each enabled saved machine. `enter` opens a read-only mirror beside your current pane, `c` a mirror you drive, and `p` cycles the size policy for driven mirrors (default `follow`). `tab` switches to your saved views; `enter` opens one as a new tab and `d` deletes it.

**Save a view** (`prefix+shift+m`). Names the current tab and saves its layout and mirrors. Other panes come back as shells in their saved directories.

**In a mirror.** The bottom row says where the mirror comes from and how it runs. `ctrl+]` closes it. In a driven mirror, keys, the mouse wheel and clicks go to the source; Shift+Enter arrives as Escape-Enter, which agents such as Claude Code read as a new line.

**Run a herd.** From a shell pane in Herdr:

```bash
remuda herd --machine ziggy --count 3 --cwd ~/code/project
```

This starts 3 Claude Code agents (`herd-1`…`herd-3`) on ziggy, one tab each in a new `herd` workspace, and turns your pane into a board of their states. When an agent turns blocked (red), a driven mirror of it opens in this tab and Herdr shows a toast. Answer it there; once the agent is working again for a moment, the mirror closes and frees its seat. At most 3 seats open at once (`--seats`), as many as fit: they split right while columns stay 70 wide, then down while panes stay 14 tall, so the board stays top-left. Agents that are blocked with no free seat wait in line, longest-waiting first. Closing a seat yourself (`ctrl+]`) leaves that agent alone until it asks again.

`q` closes the board and its seats; the agents keep running, and `remuda herd --machine ziggy --workspace <id>` watches them again (it prints the id). `--kind` picks another agent, `--name` the name prefix, and arguments after `--` go to the agent. A kind Herdr cannot start itself, such as `auggie`, is run in the tab's shell, and its Herdr plugin (for auggie, [herdr-auggie](https://github.com/tlpinney/herdr-auggie)) must be installed on the machine to report its state. Until such an agent first reports, which can wait on a startup screen, the board shows it as `startup` after 8 seconds and gives it a seat.

From a shell inside Herdr:

```bash
remuda view --source w9:p1 --machine ziggy --mode control --size follow
remuda herd --machine ziggy [--count 3] [--kind claude] [--cwd DIR] [--seats 3] [--workspace ID] [-- AGENT_ARGS]
remuda views list
remuda views save <name> <tab_id>
remuda views open <name> [--workspace <id>] [--no-focus]
remuda views delete <name>
```

## Agent skill

remuda ships a skill at [`skills/remuda/SKILL.md`](skills/remuda/SKILL.md) that teaches a coding agent running in a Herdr pane to open mirrors and views and to run herds. Install it the way Herdr's own skill installs:

```bash
npx skills add tlpinney/remuda --skill remuda -g
```

`-g` installs it for your supported agents globally; leave it out to install it into the current project. `remuda --skill` prints the copy built into your remuda binary.

## Configuration

`config.toml` in the plugin config directory (`herdr plugin config-dir remuda`):

```toml
# How this machine appears on panes it mirrors (default: the host name).
name = "travis-air"
```

## How it works

A mirror shows its source through a read-only `terminal session observe` stream sized to the mirror, so watching never resizes anything. A driven mirror adds a `terminal session control` stream for input, and the size it requests is set by the resize policy. Remote streams run the remote `herdr` over SSH.

Each mirror reports itself to Herdr as an agent, with the source's state and a resume command, so the sidebar shows it and Herdr reopens it after a restart. Its settings travel as `remuda_*` pane metadata tokens, which saved views read back. It marks the source pane through `herdr --machine` with a TTL that it refreshes while it runs, so a mirror that dies leaves no lasting mark.

All remote calls to a machine share one SSH control connection, started once with your SSH config; later calls attach to it without rereading the config, which keeps a slow `Match exec` from costing seconds per call. When it cannot start, calls fall back to `herdr --machine` and plain `ssh`.

When a stream drops, the mirror keeps its last frame under a "reconnecting" bar and retries with backoff. When the machine answers but the source pane is gone, it says so and closes itself after a minute unless the pane comes back. At startup (a restart or a saved view) it finds a source that moved to a new pane id by its label, agent, title and directory; a reconnect only looks for the same pane.

### Limits

- **Kitty keyboard keys.** Herdr's `terminal session` stream does not report the source's keyboard mode, so a driven mirror sends classic key encodings. Shift+Enter is translated to Escape-Enter; other kitty-only key distinctions are lost.
- **SSH sessions.** sshd allows 10 sessions per connection by default (`MaxSessions`). Each driven mirror holds two and each read-only mirror one, so about five driven mirrors of one machine reach the limit.

## Releasing

Bump `version` in both `Cargo.toml` and `herdr-plugin.toml`, commit, then push a matching tag:

```bash
git tag v0.2.0
git push origin v0.2.0
```

The release workflow runs the checks, builds the four binaries, records [build provenance](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations) for them, and creates a **draft** release with `SHA256SUMS`. Then sign and publish it from a machine holding the release key:

```bash
just sign-release v0.2.0
```

CI never has the signing key, so a compromised workflow cannot produce a release the install step accepts. Releases are immutable once published.

## Security

- **Release signatures.** See Install and Releasing. To check a download by hand:

  ```bash
  ssh-keygen -Y verify -f signing/allowed_signers -I tlpinney -n remuda-release -s SHA256SUMS.sig < SHA256SUMS
  shasum -a 256 -c SHA256SUMS --ignore-missing
  gh attestation verify remuda-macos-aarch64.tar.gz --repo tlpinney/remuda
  ```

- **Trust root.** The signing key list ships in this repository, so it protects against tampered releases, not against a compromised repository. Pin a reviewed commit with `herdr plugin install tlpinney/remuda --ref <commit>` if that matters to you.
- **What a driven mirror can do.** A driven mirror types into the source pane with your keystrokes, over your own SSH access. Read-only mirrors never send input.

## License

Apache-2.0, the same license as Herdr. See [LICENSE](LICENSE).
