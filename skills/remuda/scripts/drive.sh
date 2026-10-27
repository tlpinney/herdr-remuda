#!/bin/sh
# Prompts every agent in a remote herd workspace after random delays, so herd
# seats open and close. Each agent gets ROUNDS prompts, each sent only once
# the agent is idle again.
#
# Usage: drive.sh MACHINE WORKSPACE [ROUNDS] [PROMPT]
set -eu

machine=$1
workspace=$2
rounds=${3:-4}
prompt=${4:-"Use your ask-user tool (or the tool you have for asking me a question) to ask me a short, fun question with two or three options. After I answer, ask me one more, different question the same way. Then reply with one short sentence about my answers."}

status() {
  herdr --machine "$machine" pane get "$1" 2>/dev/null |
    python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["pane"].get("agent_status",""))' 2>/dev/null ||
    true
}

panes=$(herdr --machine "$machine" pane list | python3 -c '
import json, sys
for pane in json.load(sys.stdin)["result"]["panes"]:
    if pane.get("workspace_id") == sys.argv[1] and pane.get("agent"):
        print(pane["pane_id"])
' "$workspace")
[ -n "$panes" ] || { echo "no agents in $workspace on $machine" >&2; exit 1; }

drive() {
  pane=$1
  round=1
  while [ "$round" -le "$rounds" ]; do
    sleep $((15 + $(od -An -N1 -tu1 /dev/urandom) % 31))
    while :; do
      case $(status "$pane") in idle | done) break ;; esac
      sleep 3
    done
    echo "$(date +%T) $pane round $round"
    herdr --machine "$machine" pane send-text "$pane" "$prompt" >/dev/null
    sleep 1
    herdr --machine "$machine" pane send-keys "$pane" enter >/dev/null
    sleep 10
    round=$((round + 1))
  done
  echo "$(date +%T) $pane finished"
}

for pane in $panes; do
  drive "$pane" &
  sleep $((5 + $(od -An -N1 -tu1 /dev/urandom) % 10))
done
wait
