# Evidence workflows

Use this reference only for tools advertised by the connected server through
the local `--native-tools` mode. They are not part of the default three-tool
PRACTICE interface.

Use these tools for repeated named observations, offline comparison or bounded
firmware readback, not to control execution or classify Boot/App automatically.
The connected server schema is authoritative. Choose addresses and expectations
from the current project's approved map, not from these examples.

## Named observations

### `capture_snapshot`

```json
{
  "items": [
    {"name": "state", "query": {"kind": "status", "expected_state": 3}},
    {"name": "running", "query": {
      "kind": "expression", "expression": "STATE.RUN()", "expected_value": "TRUE()"
    }}
  ],
  "save_artifact": false
}
```

Use 1..64 unique item names. Query kinds and expectations:

| Kind | Required input | Optional expectation |
| --- | --- | --- |
| `status` | none | `expected_state` |
| `expression` | `expression` | exact native text `expected_value` |
| `register` | `register` | unsigned 64-bit numeric `expected_value` |
| `symbol` | exact `symbol` name | 32-bit address string `expected_address` |
| `memory` | `address`, `length` | `expected_hex` |

Memory also accepts `access` and `dual_port` with the same caps and state
guard as `read_memory`: at most 4096 bytes/item and 65536 bytes total; running
requires explicit `arm_dap` plus `dual_port: true`. Expressions/MMIO remain
side-effectful. PER is not a snapshot query kind; use its dedicated batch tools.

Each item has independent `read.ok` and a `check` status of `pass`, `fail`,
`not_requested` or `incomplete`. A failed expectation is not a failed read.
Preserve incomplete/error items and per-item timing. `non_atomic` remains true,
including on a stopped target. Do not halt to repair unavailable PC/SP reads.

## Offline comparison

### `diff_snapshots`

```json
{
  "before_id": "0123456789abcdef0123456789abcdef",
  "after_id": "fedcba9876543210fedcba9876543210"
}
```

Replace example IDs with actual `artifact.id` values from two captures saved
with `save_artifact: true`, only when evidence persistence is requested.
Snapshots must belong to the same configured artifact directory. This call
does not connect to the debugger and accepts IDs, not arbitrary file paths.

Comparison matches names and query semantics, ignoring timestamps. Changed,
unchanged, added, removed, incompatible and unavailable are distinct outcomes.
Different access modes/ranges are not ordinary value changes. Two failed reads
must not be reported as unchanged. Expectation/check changes are separate from
changes in observed values.

## BIN/Intel HEX readback

### `verify_image`

```json
{
  "path": "E:/firmware/App.bin",
  "format": "bin",
  "base_address": "0x08010000",
  "allowed_ranges": [{"address": "0x08010000", "length": 65536}],
  "access": "program",
  "chunk_bytes": 4096,
  "timeout_ms": 30000,
  "save_artifact": false
}
```

Use an existing absolute local regular file and explicitly approved readable
ranges covering every image byte. BIN requires `base_address`; Intel HEX uses
`format: "intel_hex"` and must omit `base_address`. ELF is not supported.
Checksums, lengths, EOF, overflow and conflicting overlaps are validated before
connection. Sparse holes are not read or filled with assumed bytes.

The target must already be halted/stopped (state 1 or 2), even if `access` is
`arm_dap`; there is no running-target exception or dual-port bypass. No write,
flash, reset, halt, resume or image loading is performed. Changed/unknown
sampled states stop further chunks. Stopped state does not exclude DMA changes.

Default image limit is 1 MiB, at most 16 MiB; chunks are 1..4096 bytes and
`timeout_ms` is at most 60000. Native calls cannot be hard-interrupted.
Inspect `complete`, `matches`, checked/unchecked coverage, per-segment results,
hash scope, mismatches and `stop_reason`. `matches` is null for incomplete
readback, not true. Partial hashes are not complete target-image hashes.
Matching bytes do not authenticate firmware or prove application startup.

## Local persistence

`save_artifact` defaults to false. Enable only when requested; snapshots can
contain raw memory, expressions and secrets. Verification evidence contains
hashes, paths, ranges and mismatch addresses rather than raw target bytes.

`--artifacts` selects the server's local evidence directory. Responses include
an ID, absolute path and SHA-256 when saving succeeds. `artifact.saved: false`
can coexist with valid observations: preserve them and report the save failure.
Failed/partial captures can also be saved; do not silently replace them with a
retry. Files are not overwritten or automatically deleted.

Limits are 4 MiB/artifact, 128 entries and 128 MiB/directory. Do not delete older
evidence to evade capacity checks without authorization. Use separate
directories for concurrent processes; local files are not tamper-proof evidence
or durable running-job journals. No implicit upload/publication occurs.
