---
name: trace32
description: Debug embedded targets with Lauterbach TRACE32 and t32mcp using documented PRACTICE skills, or advertised native tools for batch inspection and evidence collection.
---

# Embedded Software Debugging with Lauterbach TRACE32

Use the configured `t32mcp` server and the script reference in this file.
The default interface has three generic tools: `execute_practice_skill`,
`collect_practice_skill_response` and `abort_practice_skill`. Capabilities are
provided by documented PRACTICE scripts, not by assuming extra MCP endpoints.

**Do not read `.cmm` contents or parse `scripts/` directories.** Use the
arguments documented here. If the connected server advertises
`list_practice_skills`, it can confirm installed names without reading scripts.

## Build your strategy around a hypothesis

Formulate one concrete hypothesis and choose the smallest relevant observation
set. Inspect the server's tool list before choosing an operation. With the
default interface, use `get_state.cmm` to observe target state. If advertised,
`doctor` or `get_debugger_status` can provide native session diagnostics.
A powered board does not imply a listening or attached PowerView session.

Inspection does not authorize halt/resume, stepping, reset, flash, image/PER
loading, writes, breakpoint changes or recovery. Do not stop a running target
merely to make a read succeed. Run-control and mutation scripts below require
authorization for that action. Reset remains the user's responsibility;
after an authorized state change, observe the state again.

A failed observation is not zero or proof of equality. Preserve errors,
truncation, partial coverage and unattempted work. Keep reports local unless
sharing is explicitly requested. Separate offline tests, live refusal checks
and successful hardware reads; a prior session's state is not current evidence.

## Keep target configuration session-specific

An advertised MCP tool confirms an interface, not hardware support. Do not
carry a project's CPU selection, SWD/JTAG setup, memory map, register names,
symbols, access modes or PER definition into another session as defaults.
Use current-session configuration and evidence; CPU identity alone is not a
capability test.

PER is chip/configuration-dependent and is not supported everywhere. Prefer
user-supplied session configuration; `--target-config` can declare `per_support`
as `auto`, `supported` or `unsupported`. `doctor` separates these declarations
from automatic `PER.FILENAME()` metadata observations. A filename confirms
only a selected definition, not working or safe register reads. An unavailable
metadata query remains unknown, not unsupported; never override an explicit
unsupported declaration by probing.

Use a compatible current-session definition and understood paths before PER
reads; otherwise report what information is missing. Do not test support with
a bulk/root PER read, load a definition automatically or substitute guessed
MMIO addresses. Continue other independently supported observations.

## Execute a documented script

For example, observe target state through the default interface:

```json
{
  "skill_name": "trace32",
  "script_name": "get_state.cmm",
  "script_args": {}
}
```

Pass these arguments to `execute_practice_skill`. The server needs `--skills`
pointing to a root that contains `skill-trace32/scripts`. The tool's skill name
is `trace32`, without the directory's `skill-` prefix; keep `.cmm` in the script
name. For another installed skill, use its documented name or, when available,
the name returned by `list_practice_skills`.

If output starts with `<NOT FINISHED>`, call
`collect_practice_skill_response` with `{}` for the same invocation; do not
restart the script to collect output. Preserve `<TRUNCATED>true</TRUNCATED>`
as incomplete evidence. Stop polling at a task-appropriate deadline and report
a still-active script rather than spin indefinitely.

`abort_practice_skill` with `{}` sends `END` to PRACTICE. Use it only for an
authorized cancellation, not to acquire the debugger from another client.
It is not a target halt or reset. `--read-only` hides execution and abort;
without native extensions, only collection remains. Report unavailable
observations rather than bypassing the selected mode. Unrestricted
`execute_practice` requires explicit `--t32chat` and is not the default path.

## Optional local native tools

The local `--native-tools` startup flag exposes 14 additional tools. These are
an opt-in local extension, not the repository's default or an upstream-approved
extension pattern. Use them only when the connected server advertises them;
do not enable another mode or restart the debugger merely to complete a query.
When available, batch related observations instead of one call per item.

| Need | Advertised tools | Reference |
| --- | --- | --- |
| Session health, registers, expressions, symbols, memory, windows, polling | `doctor`, `get_debugger_status`, `read_registers`, `evaluate_expressions`, `lookup_symbols`, `read_memory`, `read_debugger_window`, `wait_for_condition` | [Native inspection](references/native-inspection.md) |
| Several PER registers/bitfields or selected subtrees | `read_per_registers`, `read_per_views` | [Batch PER inspection](references/peripheral-inspection.md) |
| Named observations, offline comparison, BIN/HEX readback | `capture_snapshot`, `diff_snapshots`, `verify_image` | [Evidence workflows](references/evidence-workflows.md) |
| Installed script names without reading scripts | `list_practice_skills` | Use advertised schema; arguments are `{}` |

Read only the relevant reference before using these tools. Example addresses,
symbols and PER titles are not an approved map for the current target.

- Native states are `0` down, `1` halted, `2` stopped and `3` running. Errors or
  unrecognized states are unknown, not stopped.
- PER reads and image verification require an already halted/stopped target. Never
  bypass a refusal with expressions, raw PRACTICE or other memory/script helpers.
- PER batches require explicit `acknowledge_side_effects: true` for trusted
  definitions and understood selections. PER logic can execute writes; neither
  `--read-only` nor a stopped target is a hardware sandbox.
- This server implements running-target native memory reads only with explicit
  `access: "arm_dap"` and `dual_port: true` on compatible hardware with a verified
  memory map. ARM DAP is not a portable default or fallback for other chips.
  Generic dual-port access can use StopAndGo; do not substitute another mode.
- Batches are sequential and non-atomic. Matching sampled states do not rule
  out transient changes, DMA writes or another client's actions. Deadlines
  cannot cancel blocked native calls; do not silently retry failed observations.
- Save artifacts only when requested (`save_artifact: true`); snapshots can
  contain raw memory and secrets. Running-state refusals do not verify
  successful stopped-target PER or image reads.

## Script Reference

Examples below are `script_args`, not top-level tool inputs. Scripts without
listed arguments use `script_args: {}`. Every script must remain documented
here when its implementation changes.

The `scripts/` directory contains a set of scripts implementing basic debugging functionality.
Some scripts can be called with arguments.

Always try to minimize the total amount of script calls, using the arguments as good as possible.

### Run Control

#### `get_state.cmm`

Get the target system's state.

#### `set_halted.cmm`

Halt the target system.

#### `set_running.cmm`

Continue the execution for `wait` milliseconds.
Parameter is optional. If not set, the target will keep running.
**IMPORTANT: Decimal values require a trailing dot, e.g. `12.`.**

```json
{
  "type": "object",
  "properties": {
    "wait": { "type": "string" }
  },
  "required": []
}
```

Example:

```json
{ }
```

```json
{ "wait": "1000." }
```

### Core Perspective

#### `core_get_registers.cmm`

Access the core's current registers.


#### `core_set_register.cmm`

Set core register `register` to value `value`.

```json
{
  "type": "object",
  "properties": {
    "register": { "type": "string" },
    "value": {"type": "string"}
  },
  "required": ["register", "value"]
}
```

Example:

```json
{
    "register": "x1",
    "value": "0x10"
}
```

#### `core_get_instructions.cmm`

Read instructions around current program counter or a specified `address`.
**Note**: `address` can also be a symbol.

```json
{
  "type": "object",
  "properties": {
    "address": { "type": "string" }
  },
  "required": []
}
```

Example:

```json
{ }
```

```json
{ "address": "0x1000" }
```

```json
{ "address": "main" }
```

The line which the PC currently stands on is highlighted by underscores.

#### `core_step.cmm`

Execute `count` core instructions.
The parameter is optional; omit it to execute one instruction.
**IMPORTANT: Decimal values require a trailing dot, e.g. `12.`.**

```json
{
  "type": "object",
  "properties": {
    "count": { "type": "string" }
  },
  "required": []
}
```

Examples:

```json
{ }
```

```json
{ "count": "10." }
```

### System on Chip (SoC) Perspective

Since the "execution environment" of the core can change, the SoC perspective is very important.

**Note:** The following scripts are general in their names but highly specific in their returned values.
If anything relevant is missing, get back to the user and tell them the required additional information.

#### `soc_get_state.cmm`

Get extensive information about the current state of the system on chip.
This may include:

- Control and status registers of the cores
- Information about peripherals

### Memory Perspective

**Note:** For the breakpoint and trace operations below, the `address` parameter can be either:
- An absolute memory address (e.g., `"0x1000"`)
- A symbol name for functions or variables (e.g., `"main"`, `"total"`, `"static_var"`)

Using symbols is often more convenient as you don't need to manually find addresses.

#### `mem_get_address.cmm`

Read the data at the specific memory address `address`.

```json
{
  "type": "object",
  "properties": {
    "address": { "type": "string" }
  },
  "required": ["address"]
}
```

Example:

```json
{ "address": "0x1000" }
```
#### `mem_dump_address.cmm`

Dump the data from memory starting at address `address`.

```json
{
  "type": "object",
  "properties": {
    "address": { "type": "string" }
  },
  "required": ["address"]
}
```

Example:

```json
{ "address": "0x1000" }
```

#### `mem_set_breakpoint.cmm`

Set a breakpoint at address `address`, `type` indicates whether it triggers when the address is accessed for read access, write access or execution.
`address` can also be a symbol (e.g. `main` or `static_var`).

```json
{
  "type": "object",
  "properties": {
    "address": { "type": "string" },
    "type": {"type": "string", "enum": ["R", "W", "X"] }
  },
  "required": ["address", "type"]
}
```

Example:

```json
{ "address": "0x100", "type": "R" }
```

```json
{ "address": "static_var", "type": "W" }
```

```json
{ "address": "main", "type": "X" }
```

#### `mem_trace_address.cmm`

Trace accesses to memory address `address`, `type` indicates whether it traces when the address is accessed for read access, write access or execution, respectively.
`address` can also be a symbol (e.g. `main` or `static_var`).

```json
{
  "type": "object",
  "properties": {
    "address": { "type": "string" },
    "type": {"type": "string", "enum": ["R", "W", "X"] }
  },
  "required": ["address", "type"]
}
```

Example:

```json
{ "address": "0x100", "type": "R" }
```

```json
{ "address": "static_var", "type": "W" }
```

```json
{ "address": "main", "type": "X" }
```

**Tip:** To trace both read and write access to a variable, call this script twice with the same address but different types (`R` and `W`).

#### `mem_clear_breakpoints.cmm`

Clears all active breakpoints and traces.

### Software Perspective

See `mem_*.cmm` scripts to control breakpoints and tracing for functions and variables.

#### `sw_get_locals.cmm`

Get all local variables.

#### `sw_get_stack_frame.cmm`

Get the current stack frame.

#### `sw_get_source.cmm`

Read source code around current program counter or a specified `address`.
**Note**: `address` can also be a symbol.

```json
{
  "type": "object",
  "properties": {
    "address": { "type": "string" }
  },
  "required": []
}
```

Example:

```json
{ }
```

```json
{ "address": "0x1000" }
```

```json
{ "address": "main" }
```

The line which the PC currently stands on is highlighted by underscores.

#### `sw_get_symbols.cmm`

Get all symbols matching the filter `wildcard` (e.g. `*main`).

```json
{
  "type": "object",
  "properties": {
    "wildcard": { "type": "string" }
  },
  "required": ["wildcard"]
}
```

Example:

```json
{ "wildcard": "*main"}
```

```json
{ "wildcard": "static_var"}
```


#### `sw_step_over.cmm`

Step over the next `count` lines. Omit `count` to perform one step.
**IMPORTANT: Decimal values require a trailing dot, e.g. `12.`.**

```json
{
  "type": "object",
  "properties": {
    "count": { "type": "string" }
  },
  "required": []
}
```

Examples:

```json
{ }
```

```json
{ "count": "10." }
```

#### `sw_step_into.cmm`

Step into the next function

#### `sw_get_source_file.cmm`

Returns the contents of `filename`.

```json
{
  "type": "object",
  "properties": {
    "filename": { "type": "string" }
  },
  "required": ["filename"]
}
```

Example:

```json
{ "filename": "main.c" }
```

```json
{ "filename": "lib.h" }
```

#### `sw_fgrep_source.cmm`

Searches all source files for occurrences of `pattern`.
**Important**: `pattern` is NOT a wildcard and NOT a regex.

```json
{
  "type": "object",
  "properties": {
    "pattern": { "type": "string" }
  },
  "required": ["pattern"]
}
```

Example:

```json
{ "pattern": "main"}
```

```json
{ "pattern": "static_var"}
```

```json
{ "pattern": "&&"}
```

#### `sw_set_breakpoint.cmm`

Set a breakpoint to `line` of `file`.

```json
{
  "type": "object",
  "properties": {
    "file": { "type": "string" },
    "line": {"type": "string" }
  },
  "required": ["file", "line"]
}
```

Example:

```json
{ "file": "main.c", "line": "13." }
```

```json
{ "file": "lib.h", "line": "10." }
```

**IMPORTANT: Decimal values require a trailing dot, e.g. `12.`.**
