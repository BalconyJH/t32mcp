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
**1**, not the connected board's port. Default-interface tests exercise only
the three generic PRACTICE tools; native feature tests explicitly request
`--native-tools`. Read-only and `--t32chat` combinations are tested separately.

For an installed executable and built package archives, set these variables
before running pytest from `tests`:

```powershell
$env:T32MCP = (Get-Command t32mcp).Source
$env:T32MCP_WHEEL = (Resolve-Path "..\dist\t32mcp-0.2.2-py3-none-win_amd64.whl").Path
$env:T32MCP_SDIST = (Resolve-Path "..\dist\t32mcp-0.2.2.tar.gz").Path
uv run --locked pytest
```

Use the actual artifact paths. Archive checks are skipped when their
variables are absent. Packaging checks can also run independently from the
repository root with `python .\tests\test_packaging.py`.

Use the toolchain pinned in `t32mcp-rs/rust-toolchain.toml` and the prerequisites
in [installation](../docs/installation.md).

## Opt-in hardware checks

The pre-existing Rust integration tests in `t32mcp-rs/tests/test_t32.rs` contact
PowerView. Do not use an unqualified `cargo test` for offline verification or
against a shared debugger.

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

The helpers start native inspection with `--native-tools --read-only` and do not
halt a target to make a read succeed. An unchanged sampled state does not prove
the absence of transient changes. Read [capabilities and limits](../docs/local-capabilities.md)
before the optional `hardware_inspection.py` probe; memory checks require an
explicitly approved range. Follow the separate
[PER verification procedure](../docs/peripheral-verification.md) only with
session-specific support and authorization; offline tests do not establish
successful stopped-target PER coverage.
