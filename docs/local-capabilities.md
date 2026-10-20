# Local MCP capabilities

## Design inputs

This implementation uses the installed `lauterbachdebugger-mcp` 0.1.0 as a
capability reference, not as a dependency or copied implementation. The native
Rust server remains the single executable. Its focused additions retain the
upstream 0.3.0 execution-policy tool surface by default rather than reproducing
all 51 reference tools or enabling automatic reset/flash/write operations. Native
enhancements require explicit `--native-tools`; they are a local opt-in
exception, not an upstream-approved pattern for adding capabilities.
`CONTRIBUTING.md` continues to direct new capabilities into documented scripts.

## Tool modes

| Startup options | Exposed tools |
| --- | --- |
| `--execution-policy scripts` | 3: status, instance management, script execution |
| `--execution-policy unrestricted` | 4, also command execution |
| `--execution-policy strict --skills DIR --port PORT` | 2: status and approved scripts |
| Any policy plus `--native-tools` | Policy-specific tools plus 14 native extensions |
| Any policy plus `--read-only` | 1: `trace32_get_status` |
| Any policy plus `--native-tools --read-only` | 15: status and native extensions |

An explicit execution policy is always required. `--read-only` suppresses
script/command execution and instance management under every policy.
`--t32chat` is replaced by `--execution-policy unrestricted`; it is no longer
accepted as a shortcut. Existing native clients must retain `--native-tools`.
The native tool names and result shapes remain, with an optional `port` added
to hardware requests. The server's advertised tool list establishes callable
interfaces, not support on the connected chip.

With one registered PowerView port, native tools may omit `port`. Multiple
instances require an explicit registered port; unknown ports are rejected
without probing or adding an instance. Native requests use a fresh connection
on the shared worker and disconnect afterwards, without retries or selecting a
different core. `list_practice_skills` and `diff_snapshots` remain offline.
Capability declarations apply to the server session; use separate servers when
instances need different declarations.

## Target-specific support

Do not reuse a test board's CPU selection, debug port, memory map, register
names, symbols, PER definition or access mode as a default for another session.
The server does not configure these automatically.

PER is an optional, chip/session-dependent facility, not a universal TRACE32
capability. A halted target or an advertised PER tool does not establish
support or a usable active definition. `doctor` distinguishes user declarations
from metadata observations; neither proves a successful register read. Use the
current session's configuration before choosing PER observations. Missing
definitions, unsupported operations and unavailable paths retain native errors;
an ambiguous error is not classified as proof that the chip lacks PER.

When PER is unavailable, omit it and continue independently supported
observations. Do not load a test-board definition, invent register addresses
or switch to raw MMIO as an automatic substitute.

## Session capability configuration

Optionally pass `--target-config C:\path\to\target.json`. This bounded, strictly
validated JSON file declares capabilities for the current session; it does not
contain or execute CPU selection, access-mode changes or PRACTICE commands:

```json
{"per_support": "auto"}
```

| `per_support` | Behavior |
| --- | --- |
| `auto` (default) | `doctor` queries only `PER.FILENAME()` after connection, preserving metadata and unknown read support. |
| `supported` | Records `user_declared_supported`, not observed success; `doctor` still reports definition metadata and any conflicting error. |
| `unsupported` | Records `user_declared_unsupported`; skips PER metadata and refuses both PER tools before connection or target access. |

Automatic detection is deliberately limited: a nonempty filename establishes
`active_definition.status: "present"`, an empty string establishes `"absent"`,
and a failed/unrecognized query remains `"unknown"`. It never reads the named
file, evaluates a register, renders a PER view, initializes conditions or
guesses support from the CPU name. Definitions may exist without usable paths
or safe reads; missing metadata is not proof that the chip lacks PER.

`doctor.capabilities.per` includes the declaration source and metadata evidence.
Metadata is sampled on each invocation, not cached across target changes.
`auto` does not block an explicitly requested PER operation merely because
metadata was unavailable; its actual result retains native errors. All policies
retain side-effect acknowledgement and stopped-state guards.

Config is loaded at server startup. Update it and restart the MCP server when
switching sessions/targets. The tool list remains stable; a configured refusal
is not an access-control sandbox for arbitrary expressions or PRACTICE.

## Workflow goals

- Distinguish MCP response timeouts from waiting for target conditions.
- Batch independent reads and expected-byte comparisons without losing errors.
- Verify local images against explicit allowed ranges on a stopped target.
- Retain failed and partial observations instead of replacing them with retries.
- Provide a locally installable executable with structured MCP responses.
- Keep script execution separate: scripts can erase or write target regions.

These workflows prescribe no chip, port, memory map or register set.

The local evidence tools (`doctor`, `capture_snapshot`, `diff_snapshots`,
`verify_image`) are documented in [evidence workflows](evidence-workflows.md).
They remain available with `--native-tools --read-only`; none performs
automatic run control.

See [batch PER inspection](peripheral-inspection.md) for `read_per_registers`
(up to 64 exact register/bitfield paths) and `read_per_views` (up to 16 selected
subtree pages). Both require `acknowledge_side_effects: true` and an already
halted/stopped target. They use the current default PER without loading files,
changing Watch lists or refreshing conditions. PER logic can perform writes:
neither the stopped-state guard nor `--read-only` is an interpreter sandbox.

## Starting safely

```powershell
t32mcp --execution-policy scripts --native-tools --read-only
```

Use this command as the stdio transport of an MCP client. PowerView must already
be running with `RCL=NETTCP` and the matching `PORT` setting. A powered target
without a listening PowerView process is insufficient. The server defaults to
`20000`; pass `--port` for the port configured in the current session.

The endpoint is deliberately local (`127.0.0.1`). Connection preflight fails
quickly when the port is absent. A query establishes and attaches the Remote
API, but never sends `Go`, `Break`, reset, flash, ELF-load or memory-write
commands. No implicit target connection occurs at server startup.

`--read-only` is a command-surface restriction, not an authorization boundary.
It hides `trace32_run_script`, `trace32_run_command`, and
`trace32_manage_instances`. With `--native-tools`, expression evaluation, memory and
window reads remain available. Read-to-clear peripherals and target-specific
expression functions
can have side effects; use only known observation expressions and safe memory
regions. Batched reads are sequential, not an atomic snapshot of a running
target. Do not infer target consistency from one batch response.

Emulation/dual-port (`E:`) access is not inherently nonintrusive: a configured
`SYStem.MemAccess.StopAndGo` can briefly stop and restart execution. Memory
inspection therefore rejects running-target requests unless they explicitly use
`arm_dap` with `dual_port: true` (direct CoreSight `EDAP`, not generic `E:` or
`ED:` access). Plain `DAP` is not the explicit runtime access form and is also
rejected while running. It never changes the configured memory-access mode or
falls back to a different one. Equal before/after states only establish equal
sampled states, not the absence of transient state changes. This guard does not
make arbitrary expression evaluation or MMIO reads side-effect-free.

This is a limit of this server's implementation, not a claim that every target
supports ARM DAP/EDAP. Use that explicit mode only on a compatible target with
verified support. On other architectures, running-target memory inspection
through this implementation remains unavailable; it does not auto-select DAP
or reconfigure the debugger.

HTTP defaults to loopback and supports the upstream token and Origin checks.
Non-loopback binding requires a token and an allowed origin. See
[HTTP configuration](../README.md#serving-remote-mcp-via-http).
All clients share the worker and registered PowerView instances. Native and
upstream operations are serialized on that worker; the vendor API boundary
also serializes calls and checks its thread-local channel. Authentication does
not make this a multi-tenant debugging service.

## Structured inspection examples

Successful inspection calls include MCP `structuredContent` as well as its text
representation. General inspection batches include `count`, `succeeded`,
`failed` and ordered `results`; versioned workflows such as PER use their own
count/status schemas. An individual `ok: false` does not discard other results.
Invalid requests fail before any connection or hardware operation.

`get_debugger_status` takes optional `port`. Target state values are `0` down,
`1` halted, `2` stopped and `3` running. Optional CPU/version queries retain
their own error when unavailable.

`evaluate_expressions`:

```json
{"expressions":["STATE.RUN()","Register(PC)","PRACTICE.StackDepth()"]}
```

Up to 64 expressions, each at most 4096 bytes. NUL, control characters and
semicolon command separators are rejected. Values preserve TRACE32 formatting
and native `result_type`; a decimal value can end in a dot.

`read_registers` and `lookup_symbols`:

```json
{"names":["PC","SP"]}
```

`PC` and `SP`, including `Register(PC)` above, are architecture-specific
examples. Choose names supported by the selected target; no register name is
assumed portable. For symbol lookup, use exact loaded symbol names.
Lookup uses the native 32-bit symbol API; no ELF is loaded and no address is
silently truncated by an input parser. This is not a wildcard symbol search.
The `access` result preserves the raw legacy third output field, which the
current vendor manual calls reserved; do not assume it is portable access-class
metadata. A missing symbol is identified by the documented address/size
sentinel even when the native return code is zero.

`read_memory`:

```json
{
  "regions": [
    {
      "address": "0x20000000",
      "length": 4,
      "access": "data",
      "dual_port": true,
      "expected_hex": "00 00 00 00"
    }
  ]
}
```

The address is an example, not a recommendation for a particular board.
Verify its memory map first. Addresses are unsigned 32-bit decimal digits or
`0x`-prefixed hexadecimal. Bounds are 64 regions, 4096 bytes per region and
65536 bytes per batch. Access choices are `data`, `program` and `arm_dap`.
Dual-port support depends on the target and debugger configuration; no
fallback halts the target. The example requires an already halted/stopped
target. A running target accepts only explicit `arm_dap` with `dual_port: true`;
down or unknown target states are rejected. The state check precedes each
region but cannot prevent another client or the target from changing state.
Only use DAP with a verified memory map and supported hardware.

Native reads use the documented `T32_ReadMemoryObj` address/buffer API, with
explicit `D`, `P` or `DAP` access strings. They do not use the unsupported
`T32_ReadMemoryEx` entry point or silently fall back to a legacy call.
Expected bytes must exactly match the requested length. Results contain hex,
hexdump, comparison outcome and bounded mismatch offsets. A comparison mismatch
is not an API read failure.

`read_debugger_window`:

```json
{"window":"frame","offset":0,"max_bytes":4096}
```

Choices are `register`, `frame`, `var_local`, `break_list`, `task_list`.
Pagination uses native byte offsets, not UTF-8 string length or line numbers.
Read at most 8192 bytes per call and follow `next_offset` while
`may_have_more` is true. A short nonempty page can have a following page; only a
zero-byte read establishes EOF. OS awareness, symbols and PowerView version determine
which windows are available. Unsupported windows return an error; the server
does not silently run a substitute command or create printer files.

`wait_for_condition`:

```json
{
  "expression": "STATE.RUN()",
  "expected_value": "TRUE()",
  "timeout_ms": 5000,
  "interval_ms": 100
}
```

First use `evaluate_expressions` to obtain the exact representation expected
from your expression. Matching is string equality, not numeric coercion.
The timeout is 1..60000 ms; polling interval is 20..60000 ms. Responses retain
attempts, elapsed milliseconds, the latest sample, error samples and the most
recent 1000 observations with a dropped-sample count. A missed condition returns
`matched: false` and `timed_out: true`, not a fabricated success.

**Timeout limitation:** the native TCP Remote API does not implement its
`TIMEOUT=` setting. Polling deadlines are checked between synchronous API
calls; a blocked native call cannot be interrupted by Tokio. A match returned
after a slow call can have `matched: true` and `deadline_exceeded: true`.
Other tool calls wait behind an active condition poll. Upstream script timeouts
do not provide a hard FFI timeout for native tools. Restart the
MCP process if the vendor API itself becomes unresponsive.

## PRACTICE compatibility and limits

Upstream 0.3.0 replaces execute/collect/abort with `trace32_run_script`, which
accepts optional `script_args` and instance/core selection. Under `scripts` or
`unrestricted`, address scripts by absolute `script_path`; under `strict`, use
the actual directory's `skill_name` and a `script_name` within its `scripts/`
directory. It returns output directly; the old polling markers and schema no
longer apply. `trace32_run_command` is only exposed under `unrestricted`.
The `strict` policy requires a configured skill root and an interface-1 header
declaring `"strict": true`. See [execution policies](../README.md#execution-policies)
and the bundled skill creator before migrating scripts.

The optional native `list_practice_skills` tool lists filenames and instruction
paths under the configured skills roots without parsing scripts. Discovery is
bounded and reports `truncated`; it does not certify strict eligibility or safe
execution. The new bundled skill is `lauterbach-trace32-debug-basic`.
`skill_name` is always the actual directory name, including a legacy `skill-`
prefix. `legacy_name` records the old shortened name only as migration metadata;
do not use it as an execution selector.
Review a skill's documented behavior before enabling execution.

## Verification

Offline tests do not need a board:

```powershell
Set-Location .\t32mcp-rs
cargo test --lib --bin t32mcp
Set-Location ..\tests
uv run pytest
```

Use the pinned toolchain and prerequisites in [installation](installation.md).

The Rust tests in `t32mcp-rs/tests/t32_test.rs` contact PowerView
and print output. Do not run an unqualified `cargo test` against a shared
debugger; the offline command above intentionally excludes them.

An explicit read-only smoke script is provided:

```powershell
Set-Location .\tests
uv run python .\hardware_smoke.py --port $Trace32Port
```

Set `$Trace32Port` to the current session's configured Remote API port first.
By default it queries status twice. Add repeatable `--register NAME` and
`--symbol NAME` only for names appropriate to this target. It sends no
halt/resume/reset/write/script commands. Check the generated JSON, including
per-item errors and `state_unchanged`; a powered board is not evidence that
every API capability works.

The hardware helpers explicitly start the server with
`--execution-policy scripts --native-tools --read-only`; they do not change the default server mode.

For bounded, checkpointed checks, use the opt-in inspection probe. Choose an
output path for each attempt; it records calls before and after execution,
including transport failure. These examples do not set up or restart PowerView:

```powershell
.\tests\.venv\Scripts\python.exe .\tests\hardware_inspection.py `
  --server $T32McpExecutable --port $Trace32Port `
  --phase queries --skills .\skills `
  --output .\dist\queries.json

.\tests\.venv\Scripts\python.exe .\tests\hardware_inspection.py `
  --server $T32McpExecutable --port $Trace32Port `
  --phase windows --window break_list --output .\dist\windows.json
```

Set `$T32McpExecutable` to the executable being verified. Add `--symbol NAME`
only for an exact loaded symbol and omit `--skills` if discovery is not needed.
The memory phase additionally requires an explicit `--address` and
`--length`; independently verify that the range is safe before invoking it.
For an already running supported Arm target, specify both `--access arm_dap`
and `--dual-port` for explicit EDAP. Optional `--compare` re-reads the same range with
matching and intentionally mismatching expected bytes; it never writes them.

Outcomes distinguish `pass`, `fail`, `unsupported` and `incomplete`.
`state_unchanged` is `null` when either state observation is unavailable, rather
than treating absence of evidence as a state change or a pass. Windows default
to two bounded pages, so inspect `more_available` before claiming complete
window content. Other window types may query target state/memory and are not
automatically tested by the default `break_list` probe.
