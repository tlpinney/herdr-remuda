---
name: remuda
description: Mirror Herdr panes from this or any saved SSH machine into the user's tab with remuda, open saved views, and run herds of remote coding agents that open as mirrors only while they need input. Use when the user asks to mirror, watch or drive a pane or agent on another machine, open or save a remuda view, start several agents on a remote machine (e.g. "start 3 auggie agents on ziggy", "herd these tasks"), feed prompts to herd agents, or when mirrors or herd seats look view-only or keep reconnecting.
---

# remuda

remuda is a Herdr plugin. Its `remuda` CLI works from any shell pane inside Herdr. Check first: `test "${HERDR_ENV:-}" = 1` (inside Herdr), `command -v remuda`, and `herdr machine list --json` for machine labels (a remote must be an enabled saved machine).

## Mirrors

A mirror pane shows another pane live, from this machine (`local`) or a saved machine. `view` mode only watches; `control` mode forwards keys, wheel and clicks. `--size` picks who sets the source's size when driving: `follow` (the mirror, default for control), `source`, `smallest` or `freeze`. Find the source with `herdr pane list` (or `herdr --machine M pane list`), then open the mirror beside your pane without taking focus:

```bash
herdr plugin pane open --plugin remuda --entrypoint view --placement split --direction right \
  --target-pane "$HERDR_PANE_ID" --no-focus \
  --env REMUDA_SOURCE=w9:p1 --env REMUDA_MACHINE=ziggy --env REMUDA_MODE=control --env REMUDA_SIZE=follow
```

Leave out `REMUDA_MACHINE` for a local pane. `ctrl+]` closes a mirror. The user's own picker is `prefix+m`.

## Saved views

A view is a tab's layout with its mirrors, saved by name and reopened as a new tab:

```bash
remuda views list
remuda views save <name> <tab_id>
remuda views open <name> [--workspace <id>] [--no-focus]
remuda views delete <name>        # only when the user asks
```

## Herds

`remuda herd` starts agents on a saved Herdr machine, one tab each in a new remote workspace, and turns the pane it runs in into a board of their states. When an agent turns `blocked` (red), a driven mirror of it (a *seat*) opens in that tab with a Herdr toast; once the agent works again for ~2 s the seat closes. Up to `--seats` (default 3) seats open, splitting right while columns stay 70 wide, then down while panes stay 14 tall; the board stays top-left. The user answers agents in the seats.

### Start a herd

The board takes over the pane it runs in, so never run it in your own pane. Open a new tab, run it there, and tell the user which tab to switch to:

```bash
tab=$(herdr tab create --workspace "$HERDR_WORKSPACE_ID" --label herd --no-focus)
pane=$(printf '%s' "$tab" | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["root_pane"]["pane_id"])')
herdr pane run "$pane" "remuda herd --machine ziggy --count 3 --kind claude --cwd ~/code/project"
```

Options: `--count N`, `--kind K` (default `claude`), `--cwd DIR` (remote path, absolute or `~`), `--name PREFIX` (agents are `PREFIX-1`…, default `herd`), `--seats N`, and agent arguments after `--`. Read the board to get the remote workspace id (`remuda herd · ziggy wR · …`):

```bash
herdr pane read "$pane" --source visible
```

- **Kinds Herdr starts** (`herdr agent start --help` lists them) use `agent start`.
- **Other kinds** (such as `auggie`) are run in the remote tab's shell. They need their Herdr plugin installed *on the remote machine* to report state (auggie: `herdr plugin install tlpinney/herdr-auggie` and `herdr plugin action invoke auggie.install-hooks`, run there). Check with `ssh <target> herdr plugin list`. Until such an agent first reports, the board shows it as `startup` after 8 s and seats it, since it is likely on a first-run screen.

### Reattach, quit, clean up

- `q` on the board closes it and its seats; the agents keep running. Reattach with `remuda herd --machine ziggy --workspace <id>`.
- The remote workspace stays until closed. Close only herds you started, and only once the user is done: `herdr --machine ziggy workspace close <id>`.

### Give herd agents work

- `herdr --machine M agent prompt <name> "…"` works for kinds Herdr starts. It refuses plugin-reported agents (`agent aug-1 is not an active named agent`); for those, type into the remote pane:

  ```bash
  herdr --machine ziggy pane send-text wR:p1 "the prompt"
  herdr --machine ziggy pane send-keys wR:p1 enter
  ```

- Never answer an agent's approval or question yourself unless the user asked you to; that is what the seats are for.
- To demo seats coming and going, `scripts/drive.sh MACHINE WORKSPACE [ROUNDS] [PROMPT]` (next to this file; also in the remuda repository) prompts every agent in the workspace after random 15–45 s delays, waiting for each to be idle first; by default it asks the agent to put two questions to the user. Run it in the background.

## When mirrors or seats look view-only or keep reconnecting

A driven mirror's (or seat's) bottom bar should read `control · <size>`. If it reads `reconnecting · input lost: ssh: connect to host …`, the machine's sshd refused a session: all remuda calls share one SSH connection per machine and sshd allows 10 sessions per connection (`MaxSessions`); each driven mirror holds 2, a view-only one 1. List the sessions on remuda's connection:

```bash
pgrep -fl "ControlPath=/tmp/remuda-$(id -u)/"
```

Fewer mirrors (for a herd, `--seats 2`), or a higher `MaxSessions` in the remote's `sshd_config` (the user's call; it needs root there), fixes it.
