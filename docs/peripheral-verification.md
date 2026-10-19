# Peripheral verification

This procedure validates batch PER behavior without assuming a chip, definition
or register map. It records no historical board results. Read
[batch PER inspection](peripheral-inspection.md) before hardware access.

## Offline checks

Build the executable using [local verification](local-verification.md), then
run from the repository root:

```powershell
Set-Location .\t32mcp-rs
cargo test --locked --lib peripherals::
cargo test --locked --lib peripheral_views::
cargo test --locked --lib capabilities::
Set-Location ..\tests
uv run --locked pytest test_peripherals.py test_capabilities.py
Set-Location ..
```

Unit tests use fake backends. MCP tests select an isolated unavailable Remote
API endpoint; ensure no debugger serves it in your environment. Coverage includes:

- Whole-request validation, bounds, literal paths and injection rejection before
  native access.
- Required acknowledgement, stopped-state guards, changes and soft deadlines.
- Independent field/selection errors, earlier values, unattempted entries,
  bounded output and page accounting.
- Strict configuration, unsupported refusal before connection, and metadata
  errors retained as unknown rather than guessed unsupported.

Mocked values do not establish PER support or harmless rendering on hardware.

## Session prerequisites

Obtain authorization for the selected session and each path or subtree. Review
its definition and chip documentation outside this procedure. No default CPU,
definition or address list is supplied. Do not load another board's definition
or substitute raw MMIO automatically.

Use `--native-tools --read-only`, a user-selected `--port` and optional
`--target-config`. Explicit `per_support: "unsupported"` refuses both PER tools
before connection and skips metadata detection. In `auto`, `doctor` queries
only fresh `PER.FILENAME()` metadata; a filename proves neither register support
nor safety. `supported` is a user declaration, not a bypass for guards.

PER definitions can perform writes and reads can clear hardware state.
`--read-only` is not a PER interpreter sandbox. Proceed only with approved
operations on an already halted/stopped target; this procedure does not
authorize halting it.

## Bounded hardware procedure

1. Collect status and `doctor` observations. Preserve metadata errors; stop if
   support or side effects remain uncertain.
2. Request one approved exact path with `read_per_registers` using the request
   shape in [batch PER inspection](peripheral-inspection.md). Set
   `acknowledge_side_effects: true` only after reviewing those effects.
3. If suitable, request a small explicit batch. Inspect each field's errors,
   attempt status and raw type/value, not only the overall outcome. Do not
   expand automatically to all peripherals.
4. Separately authorize `read_per_views` for selected subtrees and bounded pages.
   Rendering can evaluate more than visible output; response size limits do
   not bound interpreter effects.
5. Retain before/after state samples and partial results. Stop on unexpected
   changes or errors; do not retry, refresh conditions or change Watch lists
   as automatic recovery.

Calls are sequential, not atomic. Matching state samples do not exclude
transient changes. Soft deadlines stop subsequent calls, not an active blocking
native call. Client timeout alone does not prove the operation stopped.

Store build identity, requests and results privately under `dist/` or an explicit
evidence directory. Keep running-target refusal coverage distinct from actual
successful stopped-target reads; label untested paths as unverified.
