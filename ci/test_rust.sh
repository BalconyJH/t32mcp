#!/usr/bin/env bash
# Run the Rust tests for the pre-commit hook, like the CI `test` job: the unit
# tests, and explicitly opted-in PowerView tests in t32mcp-rs/tests/.
# T32SYS alone never opens a debugger socket or starts a simulator.
set -eu

# Validate even when hardware tests are disabled, before any debugger I/O.
port="${T32MCP_T32_PORT:-}"
if [[ -n "$port" ]] && { [[ ! "$port" =~ ^[0-9]{1,5}$ ]] || (( 10#$port < 1 || 10#$port > 65535 )); }; then
  echo "ERROR: T32MCP_T32_PORT must be an explicit integer from 1 to 65535." >&2
  exit 1
fi
cd "$(dirname "$0")/.."
unit_tests() {
  exec cargo test --manifest-path t32mcp-rs/Cargo.toml --locked --lib --bins
}
if [[ "${T32MCP_HARDWARE:-}" != 1 || -z "$port" ]]; then
  echo "Hardware tests require T32MCP_HARDWARE=1 and explicit T32MCP_T32_PORT: unit tests only." >&2
  unit_tests
fi
export T32MCP_T32_PORT="$((10#$port))"

listening() {
  (: > "/dev/tcp/127.0.0.1/$T32MCP_T32_PORT") 2> /dev/null
}

# Stop the simulator started below and wait until it is gone, so that the next
# run (e.g. the pytest hook) does not find it still listening.
stop_sim() {
  local pid
  pid="$(cat "$pidfile" 2> /dev/null)" || pid=
  rm -f "$pidfile"
  [ -n "$pid" ] || return 0
  kill "$pid" 2> /dev/null || return 0
  for ((attempt=0; attempt<50; attempt++)); do
    kill -0 "$pid" 2> /dev/null || return 0
    sleep 0.1
  done
  kill -9 "$pid" 2> /dev/null || true
}

if ! listening; then
  if [ -z "${T32SYS:-}" ]; then
    echo "No PowerView on port $T32MCP_T32_PORT and no T32SYS to start one: unit tests only." >&2
    unit_tests
  fi
  pidfile="$(mktemp)"
  trap stop_sim EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  ci/start_sim.sh "$pidfile"
fi

# One PowerView runs one script at a time, see t32mcp-rs/tests/README.md.
cargo test --manifest-path t32mcp-rs/Cargo.toml --locked -- --test-threads=1
