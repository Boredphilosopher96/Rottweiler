#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/../../../.." && pwd)
command_name=${1:-doctor}
output_directory=${2:-/tmp/rottweiler-tui-evidence/$(date +%Y%m%d-%H%M%S)}

cd "$project_root/packages/tui"

case "$command_name" in
  doctor)
    command -v bun >/dev/null
    test -d node_modules/@opentui/core
    test -f scripts/tui-visual-harness.ts
    printf 'Rottweiler TUI ready: bun %s, OpenTUI %s\n' \
      "$(bun --version)" \
      "$(bun -e 'import p from "./node_modules/@opentui/core/package.json" with {type:"json"}; process.stdout.write(p.version)')"
    ;;
  conversation|command-palette|approval|tools|theme-browser|settings-browser|mcp-browser)
    bun run scripts/tui-visual-harness.ts "$command_name" "$output_directory"
    ;;
  present)
    test -f "$output_directory"
    terminal_columns=$(tput cols)
    terminal_rows=$(tput lines)
    if [ "$terminal_columns" -lt 110 ] || [ "$terminal_rows" -lt 32 ]; then
      printf 'terminal must be at least 110x32, got %sx%s\n' "$terminal_columns" "$terminal_rows" >&2
      exit 1
    fi
    restore_terminal() {
      printf '\033[0m\033[?25h\033[2J\033[H'
    }
    trap restore_terminal EXIT HUP INT TERM
    cat "$output_directory"
    while :; do sleep 60; done
    ;;
  smoke)
    bun run typecheck
    bun test test/goldens/screens.test.ts --max-concurrency=1
    bun run scripts/tui-visual-harness.ts conversation "$output_directory"
    ;;
  cleanup)
    printf 'No persistent process or scratch state to clean. Evidence remains under %s.\n' "$output_directory"
    ;;
  *)
    printf 'usage: %s {doctor|conversation|command-palette|approval|tools|theme-browser|settings-browser|mcp-browser|smoke|cleanup} [evidence-dir]\n       %s present <scenario.ansi>\n' "$0" "$0" >&2
    exit 2
    ;;
esac
