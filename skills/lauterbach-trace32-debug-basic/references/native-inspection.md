# Native inspection

Use this reference only when the connected server advertises these optional
local tools (`--native-tools`). Otherwise use the default script workflow in
`SKILL.md`; do not assume the server can evaluate native requests.

Use the connected server's schema and batch related observations. These tools
do not need a skills directory. Examples are tool arguments, not TRACE32
command strings. Memory addresses, symbol names and expected values must be
adapted to the actual session; examples do not authorize hardware access.

## Session health

`doctor` and `get_debugger_status` take `{}` for a single instance. Native
hardware requests also accept `port`; in AMP sessions specify the registered
port explicitly. They inspect the selected core without switching it.
Prefer `doctor` for structured
connection/status failures and `get_debugger_status` for compact observations.
State is `0` down, `1` halted, `2` stopped, `3` running.

`doctor` makes one connection attempt and disconnects after the observation.
Inspect `connection.ok`, `outcome`, `target`, individual `observations` and
`errors`. A successful MCP call can still report `connection_failed`.
Status capabilities are observed-success or unknown, not a guarantee that memory,
register, symbol or PER access works. Do not reset, scan ports or reconnect
repeatedly based on an unavailable observation.

`capabilities.per` distinguishes `user_declared_supported`,
`user_declared_unsupported` and `unknown`. The startup `--target-config` JSON
can set `per_support` to `supported`, `unsupported` or `auto` (default).
For auto/supported policies, `doctor` may query only `PER.FILENAME()` metadata:
`active_definition` is present, absent or unknown, independently of the user's
declaration. Unsupported policy skips that query and refuses both PER tools.
No metadata query reads the file, renders a PER view or accesses registers.
Tool advertisement and CPU identity are not hardware capability tests. Use
chip/session-specific access modes; ARM DAP is not a portable fallback.

## Core registers, expressions and symbols

### `read_registers`

```json
{"names": ["PC", "SP"]}
```

This reads core/debugger registers, not PER register paths. Up to 64 names;
register names are architecture-specific, so replace `PC` and `SP` with names
supported by the selected target. Preserve full-width hexadecimal values.
Running-state reads may fail; report that limitation instead of halting or
switching to raw expressions.

### `evaluate_expressions`

```json
{"expressions": ["STATE.RUN()", "PRACTICE.StackDepth()"]}
```

Up to 64 expressions of at most 4096 bytes each. Use known observation
expressions, not arbitrary side-effectful functions. Preserve native `value`
text and `result_type`; decimal text can end in a dot. Per-item failures do not
erase successful siblings. This tool is not a sandbox and must not be used to
bypass PER, image-verification or memory state guards.

### `lookup_symbols`

```json
{"names": ["main", "Reset_Handler"]}
```

Exact loaded-symbol names only, not wildcards. No ELF is loaded. A missing
symbol is an error, not a usable address. The legacy `access` field is reserved
native metadata; do not infer a portable memory-access class from it.

## Bounded memory reads

### `read_memory`

```json
{
  "regions": [
    {
      "address": "0x08000000",
      "length": 16,
      "access": "arm_dap",
      "dual_port": true
    }
  ]
}
```

Use this example only after confirming the address map and direct CoreSight
access support. Bounds: 1..64 regions, 1..4096 bytes each, at most 65536 bytes
total. Addresses are unsigned 32-bit decimal digits or `0x` hex strings.
Allowed access classes are `data`, `program`, `arm_dap`.

Running targets accept only explicit `arm_dap` plus `dual_port: true` (EDAP).
Generic `E:`/`ED:` or plain DAP can be intrusive and are not substitutes. Down
and unknown states are rejected. State checks are sampled, not a lock against
other clients or hardware changes. Even EDAP does not make MMIO side-effect-free.

Optional `expected_hex` must contain exactly `length` bytes. Results preserve
hex/hexdump, read failures and comparison mismatches separately. Do not treat
an unexpected byte value as a transport failure or silently reread it away.

## Window pages

### `read_debugger_window`

```json
{"window": "frame", "offset": 0, "max_bytes": 4096}
```

Choices: `register`, `frame`, `var_local`, `break_list`, `task_list`. PER uses
the dedicated batch tools, not an arbitrary window command. OS awareness,
symbols and debugger configuration determine availability.

Pages accept 1..8192 bytes. Continue using native `next_offset` while
`may_have_more` is true, including after a short nonempty page. A successful
zero-byte page alone is EOF. Do not compute offsets from decoded text length.
Repeated pages can change; stop on unavailable/error pages and report them
rather than substituting a printer file or different command.

## Condition polling

### `wait_for_condition`

```json
{
  "expression": "STATE.RUN()",
  "expected_value": "TRUE()",
  "timeout_ms": 5000,
  "interval_ms": 100
}
```

Obtain the exact native representation before choosing `expected_value`.
Matching is string equality, not numeric coercion. Timeout is 1..60000 ms and
interval is 20..60000 ms. Results retain attempts, recent samples, errors,
elapsed time and whether the condition matched or timed out.

This is observation, not a start/resume command. Deadlines apply between
synchronous native calls; blocked FFI cannot be cancelled by this deadline.
A match can arrive after the deadline. Preserve both the match and
`deadline_exceeded` evidence. Native operations share the server's operation
lock, so another request cannot be assumed to interrupt an active poll.
