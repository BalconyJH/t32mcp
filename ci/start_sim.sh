#!/usr/bin/env bash
# Start the headless TRACE32 RISC-V simulator (tests/sim.t32) in the background
# and wait until its Remote API accepts connections. For the tests in
# t32mcp-rs/tests/. The Python tests in tests/ start it themselves.
#
# Usage: ci/start_sim.sh [pidfile]
#   Needs T32MCP_HARDWARE=1, an explicit T32MCP_T32_PORT, and T32SYS.
#   Refuses an occupied port. Writes only its own PID to pidfile, if given,
#   so the caller can stop it again. A failed start stops its own process.
#   Linux (x86_64, aarch64), macOS and Windows (Git Bash).
set -eu

# Validate before any debugger socket is opened or simulator is started.
port="${T32MCP_T32_PORT:-}"
if [[ -n "$port" ]] && { [[ ! "$port" =~ ^[0-9]{1,5}$ ]] || (( 10#$port < 1 || 10#$port > 65535 )); }; then
  echo "ERROR: T32MCP_T32_PORT must be an explicit integer from 1 to 65535." >&2
  exit 1
fi
if [[ "${T32MCP_HARDWARE:-}" != 1 || -z "$port" ]]; then
  echo "ERROR: simulator startup requires T32MCP_HARDWARE=1 and explicit T32MCP_T32_PORT." >&2
  exit 1
fi
if [[ -z "${T32SYS:-}" ]]; then
  echo "ERROR: T32SYS must name the simulator installation." >&2
  exit 1
fi
export T32MCP_T32_PORT="$((10#$port))"
if (( $# > 1 )); then
  echo "Usage: ci/start_sim.sh [pidfile]" >&2
  exit 1
fi
pidfile="${1:-}"
if [[ -n "$pidfile" && "$pidfile" != /* && ! "$pidfile" =~ ^[A-Za-z]:[/\\] ]]; then
  pidfile="$PWD/$pidfile"
fi

listening() {
  (: > "/dev/tcp/127.0.0.1/$T32MCP_T32_PORT") 2> /dev/null
}
if listening; then
  echo "ERROR: port $T32MCP_T32_PORT is occupied; refusing to start a simulator." >&2
  exit 1
fi

# The RISC-V simulator of the TRACE32 installation for this platform. Keep in
# sync with powerview_executable() in tests/conftest.py.
case "$(uname -s)-$(uname -m)" in
  Linux-aarch64|Linux-arm64) exe=bin/linux-aarch64/t32mriscv ;;
  Linux-*) exe=bin/pc_linux64/t32mriscv ;;
  Darwin-*) exe=bin/macosx64/t32mriscv-qt ;;
  MINGW*|MSYS*|CYGWIN*) exe=bin/windows64/t32mriscv.exe ;;
  *)
    echo "ERROR: no TRACE32 simulator known for $(uname -s) $(uname -m)." >&2
    exit 1
    ;;
esac

cd "$(dirname "$0")/.."

# Keep output for diagnostics without overwriting another run's log.
log="$(mktemp "${TMPDIR:-/tmp}/t32mcp-sim-$T32MCP_T32_PORT.XXXXXX.log")"
pid=
stop_failed_start() {
  [[ -n "$pid" ]] || return 0
  if kill -0 "$pid" 2> /dev/null; then
    kill "$pid" 2> /dev/null || true
    for ((attempt=0; attempt<50; attempt++)); do
      kill -0 "$pid" 2> /dev/null || break
      sleep 0.1
    done
    kill -9 "$pid" 2> /dev/null || true
  fi
  wait "$pid" 2> /dev/null || true
}
trap stop_failed_start EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
"$T32SYS/$exe" -c tests/sim.t32 > "$log" 2>&1 &
pid=$!

for ((attempt=0; attempt<150; attempt++)); do
  if ! kill -0 "$pid" 2> /dev/null; then
    echo "ERROR: PowerView exited before it listened on port $T32MCP_T32_PORT. Its output ($log):" >&2
    cat "$log" >&2
    exit 1
  fi
  if listening && kill -0 "$pid" 2> /dev/null; then
    if [[ -n "$pidfile" ]]; then
      echo "$pid" > "$pidfile"
    fi
    echo "PowerView (pid $pid) listens on port $T32MCP_T32_PORT (log: $log)."
    trap - EXIT INT TERM
    exit 0
  fi
  sleep 0.2
done
echo "ERROR: PowerView does not listen on port $T32MCP_T32_PORT after 30 s. Its output ($log):" >&2
cat "$log" >&2
exit 1
