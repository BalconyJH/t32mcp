# t32mcp Integration Tests

## Offline regression tests

Build the server before running pytest. From the repository root:

```powershell
Set-Location .\t32mcp-rs
cargo build --locked
cargo test --locked --lib --bin t32mcp
Set-Location ..\tests
uv run --locked pytest
```

Pytest uses `t32mcp-rs/target/debug/t32mcp` (`t32mcp.exe` on Windows) unless
`T32MCP` names another executable. MCP fixtures explicitly use TRACE32 port
reservations bound on localhost but **not listening**, never a connected
board's port. Reservations remain held for the test session. HTTP fixtures use
independent ephemeral HTTP ports and bearer tokens. Execution-policy tests,
including accepted script requests, use the same isolated endpoints.

Interface tests exercise the four upstream tools under explicit
`--execution-policy unrestricted`; `scripts` and `strict` test their reduced
tool sets. Native feature tests explicitly request `--native-tools`.
`--read-only` exposes only `trace32_get_status`, plus the 14 native tools when
opted in. Removed `--t32chat` fails with migration guidance. Native instance
routing tests use test-owned TCP listeners to prove that missing/unknown
instance selection and offline catalog/diff requests do not connect.

The no-`--port` startup test first reserves default port 20000 without listening
and only lists tools. If 20000 is already occupied, it skips without probing it.
No default pytest test contacts an active PowerView.

For an installed executable and built package archives, set these variables
before running pytest from `tests`:

```powershell
$env:T32MCP = (Get-Command t32mcp).Source
$env:T32MCP_WHEEL = (Resolve-Path "..\dist\t32mcp-0.3.0-py3-none-win_amd64.whl").Path
$env:T32MCP_SDIST = (Resolve-Path "..\dist\t32mcp-0.3.0.tar.gz").Path
uv run --locked pytest
```

Use the actual artifact paths. Archive checks are skipped when their
variables are absent. Packaging checks can also run independently from the
repository root with `python .\tests\test_packaging.py`.

Use the toolchain pinned in `t32mcp-rs/rust-toolchain.toml` and the prerequisites
in [installation](../docs/installation.md).

## Opt-in hardware checks

The upstream Rust integration tests in `t32mcp-rs/tests/t32_test.rs` contact
PowerView. Do not use an unqualified `cargo test` for offline verification or
against a shared debugger.

`test_contract.py` preserves the upstream versioned script-contract fixtures;
`test_device_prompt.py` checks command execution and device-prompt changes.
Both require **two explicit environment settings** before any connection:

```powershell
$env:T32MCP_HARDWARE = "1"
$env:T32MCP_T32_PORT = Read-Host "Approved disposable PowerView Remote API port"
uv run --locked pytest test_contract.py test_device_prompt.py
Remove-Item Env:T32MCP_HARDWARE
Remove-Item Env:T32MCP_T32_PORT
```

These are not read-only checks. Use a disposable simulator/session approved
for script execution. With either setting absent, collection and default
pytest runs skip them without probing any debugger.
See [contract fixtures](contract/README.md) for their compatibility guarantee.

Hardware helpers require an explicit invocation and a session port. Set
`T32_PORT` to the Remote API port of the PowerView session you intend to inspect;
neither helper selects a connected board's port for you. From `tests`:

```powershell
$env:T32_PORT = Read-Host "Selected PowerView Remote API port"
uv run python .\hardware_smoke.py --port $env:T32_PORT
```

The smoke check requests only debugger status by default. Register names are
architecture-specific, and symbol names depend on the loaded program; none are
assumed. To opt in, replace these placeholders with names valid for the selected
target. Both options may be repeated:

```powershell
uv run python .\hardware_smoke.py --port $env:T32_PORT `
    --register REGISTER_NAME --register ANOTHER_REGISTER --symbol SYMBOL_NAME
```

Smoke success requires unchanged available before/after state samples and a
successful result for every explicitly requested register and symbol. Missing
results or inconsistent batch counts fail the check. `hardware_smoke.py` uses
the checkout's debug executable by default, as pytest does; use `--server` or
`T32MCP` to select another executable.

The helpers start native inspection with
`--execution-policy scripts --native-tools --read-only` and do not
halt a target to make a read succeed. An unchanged sampled state does not prove
the absence of transient changes. Read [capabilities and limits](../docs/local-capabilities.md)
before the optional `hardware_inspection.py` probe; memory checks require an
explicitly approved range. Follow the separate
[PER verification procedure](../docs/peripheral-verification.md) only with
session-specific support and authorization; offline tests do not establish
successful stopped-target PER coverage. Tests of the hardware helpers themselves
use fake clients and never launch a debugger.
