# Evidence workflow verification

This procedure covers `doctor`, `capture_snapshot`, `diff_snapshots` and
`verify_image`. It is not a report of completed tests or board compatibility.
See [evidence workflows](evidence-workflows.md) for request contracts.

## Offline checks

Build using [local verification](local-verification.md). From the repository root:

```powershell
Set-Location .\t32mcp-rs
cargo test --locked --lib
Set-Location ..\tests
uv run --locked pytest test_workflows.py test_capabilities.py
Set-Location ..
```

Rust library tests use deterministic backends for native workflow paths. Python
fixtures select an isolated unavailable Remote API endpoint, not a board.
Ensure that no debugger serves that endpoint in the test environment.

Coverage includes independent diagnostic errors, capability provenance,
read-versus-expectation outcomes, retained connection failures, artifact round
trips, compatible/incompatible diffs and invalid identities. Image tests cover
BIN/Intel HEX validation, checksums, address extensions, sparse ranges, explicit
allowed ranges, partial coverage and state guards. Artifact tests cover bounds
and rejected paths. None establishes successful hardware reads.

Follow [package checks](local-verification.md#package-checks) to repeat MCP
tests against the installed executable and current archives. Record actual
results and skips; an earlier build's hash is not current verification.

## Authorized diagnostics and snapshots

Obtain authorization and select a session explicitly. Use
`--native-tools --read-only` and a user-selected `--port`. Optional
`--target-config` supplies declarations, not observations. PER filename metadata
never proves safe register access; explicit unsupported policy skips that query.

Collect status, then `doctor`, then status again. Retain independent errors and
query counts. Matching endpoint samples do not exclude transient changes.

Select a small set of approved snapshot observations. Register names are
architecture-specific; symbols depend on the loaded program. There is no
default board memory range. Review expressions and addresses for side effects:
`--read-only` is not an MMIO sandbox. Do not halt or reconfigure a target to obtain
a value. Running-target memory requires explicitly supported `arm_dap` with
`dual_port: true`; this is not portable across chips.

Exercise expectation handling by changing only an expected value in a later
request, not target memory. Compare `read.ok` separately from `check` and retain
unavailable observations rather than retrying until they pass. Snapshots remain
non-atomic even on stopped targets.

## Persistence and offline differences

Saving is opt-in. Use a dedicated, trusted `--artifacts` directory and approved
non-secret observations; snapshots may retain raw target data. Record returned
IDs and hashes, then use those IDs with `diff_snapshots` without debugger access.
Distinguish observed changes from altered expectations or incompatible queries.

Local artifacts are not tamper-proof records. Quotas are not a cross-process
transaction, and same-privilege directory changes remain outside the trusted
directory model. Use separate directories for concurrent servers.

## Explicit image verification

Authorize the image and every allowed address range. Validate local BIN/Intel
HEX inputs and request bounds before target access. The target must already be
halted/stopped; the tool neither halts it nor writes the image. Do not infer
whole-image success from partial reads.

Record attempted chunks, checked bytes, mismatches, hashes and state samples.
Running, changed or unavailable state is not a successful comparison. Refusal
with no checked bytes validates a guard, not image contents. Successful
stopped-target match/mismatch and partial-read behavior require separate
hardware evidence if exercised.

Native calls are synchronous and cannot be cancelled by soft deadlines or client
timeouts. Do not automatically retry an uncertain operation. Keep private logs
under `dist/` or an explicitly selected evidence directory; record revision,
executable hash, requested scope and unverified paths without publishing
project-specific memory or configuration.
