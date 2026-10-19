# Local evidence workflows

These optional local additions require `--native-tools`. Nothing is uploaded,
published, flashed, reset or automatically halted. The default server exposes
three generic PRACTICE tools; `--native-tools --read-only` exposes 15 tools,
including all four tools below.
See [verification procedure](workflow-verification.md) for coverage and checks.
The separate [batch PER tools](peripheral-inspection.md) require explicit
side-effect acknowledgement and an already halted/stopped target. PER
definitions can execute writes; their availability in `--read-only` is not a
hardware or interpreter sandbox.

## Session diagnostics

Call `doctor` with `{}`. It makes at most one lazy connection attempt (or reuses
the current channel), then independently queries target state, PRACTICE state,
CPU and software/build metadata.

An unreachable endpoint returns a successful MCP response containing
`outcome: "connection_failed"` and structured `connection.error` with `code`,
`stage`, `message`, `operation` when known, and `actionable_hint`. Native errors
retain `native_code` where present. It does not claim that endpoint failure proves
PowerView is absent, or that an unavailable query proves lack of support.

Status capabilities are `observed_success` or `unknown`, based only on the
queries made. Memory, register, symbol and PER register reads are not probed.
The separate `capabilities.per` report combines optional `--target-config`
declarations with a fixed `PER.FILENAME()` metadata query. Explicitly disabled
PER and failed connections skip this query. A filename means only that a
definition is selected, not that its registers are supported or safe to read.
User declarations remain `user_declared_supported`/`user_declared_unsupported`,
not observed successes. The `queries` counts and overall `outcome` include
attempted metadata queries; metadata failures also appear in `errors`, while
`active_definition` preserves their details and provenance. See
[configuration and detection](local-capabilities.md#session-capability-configuration).
An advertised MCP tool does not prove hardware support, and unknown does not
mean unsupported.
No automatic reconnect, retry or target recovery is attempted. A reused channel
is not proof of current health; inspect individual observations.

## Named snapshots

Example `capture_snapshot` arguments:

```json
{
  "items": [
    {"name": "target_state", "query": {"kind": "status", "expected_state": 3}},
    {"name": "running", "query": {
      "kind": "expression", "expression": "STATE.RUN()", "expected_value": "TRUE()"
    }},
    {"name": "pc", "query": {"kind": "register", "register": "PC"}},
    {"name": "main_symbol", "query": {"kind": "symbol", "symbol": "main"}}
  ],
  "save_artifact": true
}
```

`PC` is architecture-specific, and `main` requires a matching loaded symbol.
Choose names and expected states for the selected session; omit unavailable
observations rather than substituting another board's configuration.

There are 1..64 unique item names. Supported query kinds:

| Kind | Input | Optional expectation |
| --- | --- | --- |
| `status` | none; reads numeric target state | `expected_state` |
| `expression` | `expression` | exact native text `expected_value` |
| `register` | `register` | unsigned 64-bit `expected_value` |
| `symbol` | exact `symbol` name | 32-bit `expected_address` string |
| `memory` | `address`, `length`, optional `access`, `dual_port` | `expected_hex` |

Memory has the same bounds and running-target guard as `read_memory`: at most
4096 bytes/item, 65536 bytes/snapshot; running targets require explicit `arm_dap`
and `dual_port: true`. Expressions and MMIO are not sandboxed. Choose addresses
and observation expressions using the board's approved memory map.

Each item has `read.ok` and either a value or error, plus a separate `check` of
`pass`, `fail`, `not_requested` or `incomplete`. A successful memory read with
unexpected contents does not erase other successful observations. A connection
failure retains incomplete per-item evidence without issuing further queries.
Running-target register reads may be unavailable; they are not repaired by
halting. Names such as `PC` and `SP` are not portable across architectures.

Snapshots record per-item times, monotonic elapsed durations and before/after
state observations. `non_atomic` is always true, including on a stopped target.
Matching sampled states do not exclude a transition between samples. Capture is
not a board-specific Boot/App classifier; express those checks explicitly.

The request metadata limit is 512 KiB, each serialized read result is at most
16 KiB, and a complete snapshot is at most 2 MiB. Oversized read results become
explicit per-item errors, not truncated successful values.

## Offline snapshot differences

Save two snapshots with `save_artifact: true`, then call `diff_snapshots`:

```json
{
  "before_id": "0123456789abcdef0123456789abcdef",
  "after_id": "fedcba9876543210fedcba9876543210"
}
```

Replace these example IDs with the actual `artifact.id` responses. No debugger
connection is made. Both snapshots must be in this server's artifact directory.
The comparison matches item names and query semantics, ignoring timestamps;
different addresses/lengths/access modes are incompatible, not value changes.
Results distinguish added, removed, changed, unchanged, incompatible and
unavailable items. Two failed reads are not equal evidence. Expectation changes
are reported separately from changes in observed values.

## BIN and Intel HEX readback verification

Example `verify_image` arguments:

```json
{
  "path": "E:/firmware/App.bin",
  "format": "bin",
  "base_address": "0x08010000",
  "allowed_ranges": [{"address": "0x08010000", "length": 65536}],
  "access": "program",
  "chunk_bytes": 4096,
  "max_image_bytes": 1048576,
  "timeout_ms": 30000,
  "max_mismatches": 64,
  "save_artifact": true
}
```

Addresses here are examples, not an approved map for any particular board.
Provide an absolute local regular file. BIN requires an explicit base address.
Use `format: "intel_hex"` without `base_address` for Intel HEX. Checksums, record
lengths/types, EOF, address overflow and conflicting overlaps are validated
offline before connection. Sparse holes are not read or implicitly filled with
`FF`. ELF is not supported by this first version.

`allowed_ranges` is mandatory and every image byte must fit; it is not proof
that the region is safe to read. Do not include read-to-clear peripherals or
unapproved sensitive regions. Local file validation rejects network/device/ADS
paths on Windows; local filesystem permissions remain the trust boundary.

The target must already be halted or stopped (state 1 or 2). The tool never
halts it for you. State is checked before each chunk and afterward; changed or
unavailable state makes the result incomplete. These checks cannot rule out a
transient change between samples or DMA/peripheral writes while halted.

Default image limit is 1 MiB, configurable up to 16 MiB; chunks are 1..4096
bytes. Results retain input-file SHA-256, expected and checked payload hashes,
per-segment coverage/hashes, bounded mismatch addresses, and a stop reason.
Hashes of partial coverage are explicitly scoped and never presented as a
complete target-image hash. `matches` is null unless the readback is complete.
No raw target bytes are retained in verification reports. Byte equality is not
firmware signature authentication or proof of successful application startup.

`timeout_ms` is a soft deadline checked between native calls, at most 60 seconds.
A blocked vendor call cannot be interrupted; these tools still serialize with
other native operations. Worker isolation and durable asynchronous jobs are
not part of this batch.

## Evidence persistence and limits

`save_artifact` defaults to false. Explicitly saving a snapshot can retain raw
memory and expression values; do not save secrets or key material. Verification
reports retain hashes, paths, ranges and mismatch addresses, not raw bytes.

Configure a dedicated directory at startup:

```powershell
t32mcp --native-tools --read-only --artifacts "C:\path\to\evidence"
```

Defaults: `%LOCALAPPDATA%/t32mcp/evidence` on Windows, or
`$XDG_STATE_HOME/t32mcp/evidence` (`~/.local/state/t32mcp/evidence`) elsewhere.
If no suitable environment root exists, pass `--artifacts` explicitly.
The directory is created only when saving is requested.

Artifacts use generated 32-hex-character IDs, versioned JSON envelopes, unique
filenames and SHA-256 metadata. A completed write does not overwrite an existing
file. The response includes its absolute path. A persistence failure after
capture preserves the collected result and reports `artifact.saved: false`.
Failed/partial observations can be saved; no implicit retry replaces them.

Each artifact is bounded to 4 MiB. Directory guards permit at most 128 entries
and 128 MiB, reserving space conservatively before target access. There is no
automatic deletion or eviction: archive evidence manually or choose a new
directory. Use separate directories for concurrent server processes; quota
checks do not form a cross-process transaction. Local files are not a tamper-proof
audit log; the evidence directory must be writable only by trusted processes.
A process crash before completion may leave no final artifact.
Durable request journals and campaign/attempt ledgers are deferred.
