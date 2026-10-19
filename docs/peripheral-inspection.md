# Batch PER inspection

Both tools are optional local extensions. Start the server with
`--native-tools` (add `--read-only` to hide execution tools) and confirm they
appear in its tool list. The default three-tool PRACTICE mode does not expose
them.

Tool advertisement only confirms the MCP interface. PER is chip- and
session-dependent; not every chip/configuration supports it, and a compatible
definition must already be active. Do not infer support from CPU identity,
stopped state, successful connection or another board's verification.
`doctor` can query `PER.FILENAME()` metadata or report the user's
`--target-config` declaration. A detected filename does not prove register-read
support. See [session capability configuration](local-capabilities.md#session-capability-configuration).

Use `read_per_registers` for up to 64 register or bitfield paths, or
`read_per_views` for up to 16 selected peripheral subtrees, in one MCP request.
Separate results survive individual item failures. Native operations remain
serialized: this is not parallel hardware access or an atomic snapshot.

Both tools use the current default TRACE32 PER definition. They do not load or
select a PER file, change `PER.Watch`, or run `PER.Set.CONDitions`. Conditional
definitions must already be usable in the session. No generated register map
is provided when a suitable PER definition is absent. Missing paths,
unsupported fields and unavailable views remain explicit errors.

## Safety requirements

- Use current-session configuration and definition metadata before selecting
  paths or views. If neither establishes an appropriate definition, omit PER
  from the inspection plan; do not probe support with a bulk/root view, load a
  test-board definition or fall back to guessed MMIO addresses.
- `per_support: "unsupported"` refuses the two PER tools before connection,
  retaining all entries as unattempted. A `"supported"` declaration is not
  hardware evidence and cannot bypass the remaining guards.
- Set `acknowledge_side_effects` to `true` on every request, including
  address-only requests. It defaults to `false`; omission refuses the request.
- The target must already be halted (state `1`) or stopped (state `2`). Running,
  down and unknown states prevent PER operations. The tools never automatically
  halt, resume, reset, flash, retry or change memory-access configuration.
- State checks occur between operations. A changed or unavailable state stops
  further work; returned data retains its verification status. Equal sampled
  states cannot exclude transient or concurrent target changes.
- PER definitions are executable descriptions: dynamic logic can perform
  writes, and register reads can acknowledge or clear hardware state. Trust the
  active definition and choose known-safe observations. Being stopped does not
  remove these risks. `--native-tools --read-only` exposes both tools but is not
  a sandbox for the PER interpreter.
- `timeout_ms` defaults to `5000` and accepts `1..60000`. It is a soft processing
  deadline checked between native calls, not a hard end-to-end timeout or a way
  to cancel a blocked vendor call. Timing starts in the connected batch helper,
  excluding operation-lock waits and connection setup. Requests share the
  server's operation lock.
- Output limits bound retained results, not the MMIO accesses or interpreter
  work needed to evaluate a path or render a view. Each page may regenerate the
  view. Small pages are not a hardware-side safety limit.

Requests, including every batch member, are validated before connecting.
Unknown request fields, macros, control characters and command syntax are not
accepted. These input restrictions do not sandbox the active PER definition.

## Register and bitfield batches

Call `read_per_registers` with arguments such as:

```json
{
  "paths": ["System Control.CPUID", "System Control.CPUID.IMPLEMENTER"],
  "include_value": true,
  "include_choice": false,
  "acknowledge_side_effects": true,
  "timeout_ms": 5000
}
```

These CPUID paths illustrate one definition's possible format, not a portable
chip feature or successful reads verified on the connected board. Replace
them with exact paths from the session's active PER
definition, and confirm its read semantics first. Paths are case-sensitive,
dot-separated PER hierarchy paths. A leading dot, such as `.CPUID`, requests
the native whole-file register search and can be ambiguous; prefer full paths.
Wildcards and duplicate paths are rejected.

The batch requires 1..64 paths. Each accepts 1..512 ASCII bytes, without leading
or trailing spaces. Supported characters are letters, digits, spaces, `_`, `.`,
`[`, `]`, `:`, `-`, `(` and `)`. Real titles with spaces and parentheses work.

Each path evaluates `PER.ADDRESS("<path>")` and, by default,
`PER.VALUE("<path>")`. Set `include_value` to `false` to omit the latter.
`include_choice: true` also evaluates `PER.VALUE.STRING("<path>")`, which applies
to named BITFLD choices rather than general register formatting. This separate
native evaluation may reread the target; it does not decode the previously
returned numeric value. It is disabled by default.

The versioned `t32mcp.peripheral-registers` response preserves ordered `results`,
native `value` text and `result_type` for each successful `address`, `value` and
`choice` field. Native decimal text may end in a dot. Fields distinguish `ok`,
`error`, `not_requested` and `not_attempted`. An optional choice error does not
erase a successful value. Item `partial` results preserve usable fields beside
errors or incomplete state verification. A completed batch does not mean that
every field succeeded; inspect field and item statuses.

Responses include state observations, timestamps, counts of successful,
partial, failed and unattempted items, and evaluation/state-probe counts.
Connection failures retain unattempted entries; state/deadline failures retain
earlier results and a stop reason. Native value text is limited to 4096 bytes
per field and 65536 retained bytes per batch. Oversized values become errors,
not silently truncated successes. Error text is bounded too.

## Several PER subtrees at once

Call `read_per_views` with independent selections:

```json
{
  "views": [
    {"subtree": [], "expand": false, "offset": 0, "max_bytes": 2048},
    {"subtree": ["Memory Protection Unit (MPU)"], "expand": true,
     "offset": 0, "max_bytes": 4096}
  ],
  "acknowledge_side_effects": true,
  "timeout_ms": 5000
}
```

The title must exist in the current definition. This example is not a
board-validated safe peripheral sweep; check the selected definition and
registers before expanding a subtree.

`subtree` contains literal tree-title components, not a dotted register path or
raw command. For example, `["System Control", "Nested Group"]` selects that
hierarchy and generates `PER.view , "System Control,Nested Group"`. With
`expand: true` it generates `PER.view , "System Control,Nested Group,*"`.
The comma outside the quotes omits the filename, keeping the default PER.
Do not supply commas or `*` inside components; the boolean controls expansion.

An empty `subtree` with `expand: false` generates `PER.view`, preserving default
view behavior. This does not guarantee rendering accesses no MMIO. Root
expansion (`subtree: []` with `expand: true`) is rejected. Selections default to
an empty subtree, no expansion, byte offset `0` and `4096` requested bytes.

Limits are 1..16 selections, up to 16 hierarchy components per selection,
1..128 nonblank ASCII bytes per component, 1..8192 requested bytes per page,
and 65536 requested page bytes per batch. Components accept letters, digits,
spaces, `_`, `.`, `:`, `-`, `[`, `]`, `(` and `)`. An offset plus requested page
length must fit in the unsigned 32-bit range.

Each result preserves its selection, generated command, text, native
`bytes_read`, `state_verified`, errors and continuation metadata. Pagination
uses native byte offsets, not character counts or line numbers:

1. Keep each selection's `subtree` and `expand` unchanged.
2. For a successful, state-verified nonempty page, use `next_offset` in the
   next request. A short nonempty page still has `may_have_more: true`.
3. Only a successful zero-byte page establishes EOF (`may_have_more: false`,
   `next_offset: null`). An unavailable or failed page is not EOF.
4. If post-read state verification fails, returned text can remain present but
   is not a stable successful observation. Inspect errors and stop reasons;
   continuation metadata alone is not permission to keep reading.

Separate pages and selections can observe different values or layouts. They
do not create a stable snapshot or guarantee enumeration of every register.

See [local installation](installation.md),
[local capabilities](local-capabilities.md), and
[PER verification coverage](peripheral-verification.md).
