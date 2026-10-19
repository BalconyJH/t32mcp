# Local verification

This procedure separates offline coverage from authorized hardware checks.
It is not a test-result report or a hardware compatibility list. Record fresh
results for the exact revision and executable tested.

## Offline checks

Use the pinned toolchain and prerequisites in [installation](installation.md).
From the repository root:

```powershell
Set-Location .\t32mcp-rs
cargo fmt --all -- --check
cargo test --locked --lib --bin t32mcp
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --no-run
cargo build --locked
Set-Location ..\tests
uv run --locked pytest
Set-Location ..
```

Add `--offline` to Cargo commands only when dependencies are cached. These
commands run library/binary tests and only compile integration tests. Do not
use unqualified `cargo test`: existing integration tests contact PowerView.
Python fixtures select an isolated unavailable Remote API endpoint, not a board
session. Ensure that no debugger serves that endpoint in the test environment.

Coverage includes default PRACTICE registration, native opt-in, read-only
combinations, bounded inputs, absent-endpoint errors and partial observations.
Fake backends exercise state changes and native errors, not chip compatibility.
See [PER verification](peripheral-verification.md) and
[evidence verification](workflow-verification.md) for focused checks.

## Package checks

Build local archives using [installation](installation.md). Follow
[tests/README.md](../tests/README.md) to set `T32MCP`, `T32MCP_WHEEL` and
`T32MCP_SDIST`, then rerun pytest against the installed executable. Use actual
build filenames; archive tests are skipped if their variables are absent.
Record skips instead of treating them as coverage.

Compare the installed executable with the wheel binary and check that the
source archive contains the intended sources, lock file and licenses. Retain
hashes and logs under a local `dist/` directory. Do not publish private target
observations with source changes.

## Authorized hardware checks

Obtain authorization for the selected session and operations. Select its Remote
API port explicitly. Do not select a CPU, load firmware, change debug settings
or halt/reset/resume a target merely to make a test pass. A powered board alone
is not a listening PowerView session.

Start with the status-only smoke helper in [tests/README.md](../tests/README.md).
Request registers or symbols only by names valid for that session. Follow
[capability configuration](local-capabilities.md#session-capability-configuration)
for optional `--target-config`; metadata detection is not proof of safe PER
reads. Default mode retains three PRACTICE tools. Inspection requires explicit
`--native-tools`, normally with `--read-only` for these checks.

Memory, expressions and windows require separate review of target-side effects.
`--read-only` restricts commands, not MMIO or PER interpreter effects. Matching
before/after state samples cannot rule out transient changes. Native calls are
synchronous: soft deadlines and client timeouts cannot cancel an active call.
Do not automatically retry an uncertain operation.

## Recording coverage

Record revision, executable/package hashes, toolchain, commands, exit codes,
test counts and skips. Retain hardware session details privately with requested
observations, errors and state samples. Distinguish unattempted, failed, partial
and successful observations. Mocked tests, metadata filenames and one board's
results do not establish cross-chip support or verify a subsequent build.
