# Batch PER inspection

Use this reference only when the connected server advertises `read_per_registers`
or `read_per_views` through the local `--native-tools` mode. Their absence is
not permission to emulate them with arbitrary PRACTICE commands.

Advertisement does not prove PER support. PER depends on the chip, debugger
configuration and a compatible active definition; it is not available on every
target. Neither CPU identity nor stopped state establishes it. Inspect
`doctor.capabilities.per`: user declarations and automatically detected
`PER.FILENAME()` metadata are separate evidence. A selected filename is not a
successful read. Unknown metadata can mean an unavailable query, not an
unsupported chip. Never apply configuration from another board or override
`user_declared_unsupported`.

Use these tools instead of one MCP call per peripheral. They use the current
default PER definition, not raw addresses guessed from a header. A missing
definition/path or unavailable conditional branch remains an explicit error;
do not load a file, refresh conditions or rewrite the user's Watch list to
repair it automatically. Do not emulate a refused batch through
`evaluate_expressions`, raw PRACTICE or a different package's helpers.

## Preconditions

- Use the current session's user configuration and definition metadata, not
  examples or another project's successful tests. Confirm a compatible active
  definition and understood selections before attempting reads. Native
  failures retain their evidence; ambiguous errors do not prove lack of PER.
- Confirm the current target is already halted/stopped (native state 1 or 2).
  Running, down, changing and unknown states stop further PER operations.
- Both tools require `acknowledge_side_effects: true`, even for address-only
  requests. Use a trusted PER definition and understood register selections:
  dynamic PER logic can execute writes and MMIO reads can clear status.
- `--read-only` is not an interpreter sandbox; stopped state does not remove
  side effects. Never acknowledge a bulk sweep solely because it looks like
  a read operation.
- Choose exact paths/titles from the active PER. Examples below demonstrate
  format only, not verified path availability or safe reads on this board.
  Do not infer names from the MCU model, a C header or a previous session.

If suitable selections are unknown, a bounded, unexpanded root view can help
after these preconditions are met. It still invokes the PER interpreter and
is not guaranteed to avoid MMIO. There is no static whole-chip enumeration
or approved register map supplied by these tools.

## Multiple registers and bitfields

### `read_per_registers`

```json
{
  "paths": ["System Control.CPUID", "System Control.CPUID.IMPLEMENTER"],
  "include_value": true,
  "include_choice": false,
  "acknowledge_side_effects": true,
  "timeout_ms": 5000
}
```

- 1..64 unique, exact case-sensitive paths, each 1..512 ASCII bytes.
  Full hierarchy uses dots. The native leading-dot search `.CPUID` can be
  ambiguous; prefer full paths.
- Spaces, brackets, parentheses, underscore, dot, colon and hyphen are allowed.
  Quotes, escapes, macros, controls, comma separators and wildcards are not.
- Each item evaluates `PER.ADDRESS`, optionally `PER.VALUE` (default true),
  and optionally `PER.VALUE.STRING` (default false).
- Choice text applies to named BITFLD choices, not arbitrary register display.
  It is a separate evaluation, potentially another hardware read, not decoding
  of the returned numeric value. Address/value/choice are not one atomic sample.

Inspect each `results` item and each `address`, `value`, `choice` field.
Fields distinguish `ok`, `error`, `not_requested`, `not_attempted` and preserve
native value text/type. An optional choice failure can leave a usable value
and an item with `status: "partial"`. State verification failure also makes
returned values partial rather than stable successes.

Use `counts`, `state_before`, `state_after`/`state_stable` per item,
`last_state_observation` and `stop_reason` to report coverage. Batch
`status: "stopped"` means processing stopped, not that the tool halted the CPU.
`status: "completed"` alone does not mean every field succeeded.

## Several subtrees and pages

### `read_per_views`

```json
{
  "views": [
    {"subtree": ["System Control"], "max_bytes": 2048},
    {"subtree": ["Memory Protection Unit (MPU)"], "max_bytes": 2048}
  ],
  "acknowledge_side_effects": true,
  "timeout_ms": 5000
}
```

- 1..16 selections. `subtree` is an array of literal hierarchy titles, not
  dotted register syntax, a filename or a raw command.
- At most 16 components per selection, each 1..128 nonblank ASCII bytes.
  Spaces and parentheses in real titles are supported.
- Add `expand: true` to expand that selected subtree. Root
  `subtree: []` with `expand: true` is rejected; never request global expansion.
- `subtree: []`, `expand: false` selects the default unexpanded request.
  Existing view behavior is not a promise of zero register access.
- Defaults are `subtree: []`, `expand: false`, `offset: 0`, `max_bytes: 4096`.
  Pages are 1..8192 native bytes; total requested bytes are at most 65536.
  Offset plus length must fit in unsigned 32 bits.

The server constructs the omitted-file syntax itself, for example
`PER.view , "System Control,Nested Group,*"` from components and the expansion
flag. Do not embed commas or `*` in user components.

Keep each result paired with its selection. After a successful,
`state_verified: true` page, retain `subtree`/`expand` and use its `next_offset`
for the next request. A short nonempty page still has `may_have_more: true`.
Only a successful zero-byte read establishes EOF. Errors and unverified pages
are not EOF; preserve returned text/errors but do not continue automatically.

## Limits that are not safety guarantees

Both tools are serialized, non-atomic observations. Equal before/after states
cannot exclude a transient run or peripheral/DMA changes. Native errors remain
per-item; state changes, unavailable state and deadline expiry stop later work.
No automatic condition initialization (`PER.Set.CONDitions`) occurs.

`timeout_ms` defaults to 5000 and accepts 1..60000. Timing starts inside the
connected batch helper, excluding lock waits and connection setup. It cannot
interrupt a blocked native call. Retained-value/page limits bound output,
not how much interpreter or MMIO work happens. Repeated pages may regenerate
the entire selected view and need not represent stable values or layout.

Report live successful reads separately from offline fake-backend tests and
running-state refusals; the latter do not verify stopped-target PER behavior.
