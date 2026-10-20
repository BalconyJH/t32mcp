# Local installation with uv

This repository packages the existing Rust executable with Maturin's `bin`
bindings. Installing it exposes the `t32mcp` command; there is no Python MCP
server or Python wrapper to maintain. The package version comes from
`t32mcp-rs/Cargo.toml`.

These instructions build and install local artifacts only. Nothing is published
to PyPI or another package index. Initial source builds may download build
dependencies and Cargo crates. The `Private :: Do Not Upload` package classifier
is an additional guard against accidental PyPI publication.

## Requirements

For installation from a matching prebuilt wheel:

- uv and Python 3.10 or newer.
- For debugger operations, a running TRACE32 instance with its TCP Remote API
  enabled. The server default is `20000`; use `--port` when your session uses
  another port. Installation and `--help` do not require a debugger or board.
- For skill discovery/execution, an explicit skills directory containing
  `lauterbach-trace32-debug-basic/scripts/*.cmm`. Optional native inspection tools work without
  skills, but require `--native-tools`.
  The wheel does not bundle skills. A checkout's `skills` directory is
  one option, and must remain available after installation.

For a build from this checkout or a source distribution:

- The Rust toolchain specified in `t32mcp-rs/rust-toolchain.toml` (currently
  `1.96.0`).
- On Windows x64, Visual Studio Build Tools with the C++ build tools and a
  Windows SDK. Run build commands from a Developer PowerShell for Visual Studio
  so that the MSVC linker and SDK are available.
- libclang for the Rust `bindgen` build dependency. Set `LIBCLANG_PATH` to the
  directory containing `libclang.dll` if it cannot be discovered automatically.
  LLVM's `bin` directory is a common location; use the actual path on your PC.
- On Linux, a C/C++ toolchain and libclang development/runtime libraries.

The checkout supplies Remote API libraries for Windows x64 MSVC, Linux x64
GNU, and Linux ARM64 GNU. Other targets, including macOS and Windows ARM64,
are not supported by the supplied native libraries. Build wheels on a supported
target with its matching native toolchain.

## Install from the checkout

In Developer PowerShell, adjust these paths for your checkout and LLVM install:

```powershell
Set-Location C:\path\to\t32mcp
$env:RUSTUP_TOOLCHAIN = "1.96.0"
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
uv tool install --force .
t32mcp --help
```

`RUSTUP_TOOLCHAIN` selects the crate's pinned toolchain even though the Python
project lives one directory above `rust-toolchain.toml`. This environment
variable only changes the current shell and its child processes.

After changing Rust source, rerun `uv tool install --force --reinstall .`.
Installing a native binary is not an editable source installation.

To launch the MCP server over standard input/output:

```powershell
t32mcp --execution-policy scripts --skills "C:\path\to\skills"
```

This process waits for an MCP client; a quiet terminal is expected. Do not use a
manual launch as a hardware health check. Configure your MCP client to run the
same command and arguments instead:

```json
{
  "mcpServers": {
    "t32mcp": {
      "command": "t32mcp",
      "args": [
        "--execution-policy", "scripts",
        "--skills", "C:\\path\\to\\skills"
      ]
    }
  }
}
```

These examples expose the three upstream tools allowed by the `scripts` policy.
All server launches require an explicit execution policy. `strict` additionally
requires an explicit `--port`, a skills root, and compatible script headers.
Native
inspection is an explicit local extension; add `--native-tools` to the server
arguments only when needed. For native inspection without execution tools:

```powershell
t32mcp --execution-policy scripts --native-tools --read-only
```

Set paths and the Remote API port for your environment. These launch commands
do not select a chip, attach a target, configure SWD/JTAG or load a PER file.
Native tool availability does not establish hardware support; PER is optional
and depends on the chip, PowerView configuration and active definition.

To declare PER support for this session, optionally add
`--target-config C:\path\to\target.json`. For example, `{"per_support":"unsupported"}`
disables PER metadata detection and both PER tools without target access.
Omission defaults to `auto`: `doctor` may inspect the selected definition's
filename, but does not claim successful PER reads. See
[capability configuration](local-capabilities.md#session-capability-configuration).

`--read-only` disables script execution, commands and instance management.
Without `--native-tools`, it leaves only `trace32_get_status`;
it does not enable inspection tools. It is not an MMIO sandbox. Existing local
clients that previously relied on native tools being enabled by default must
add `--native-tools` after updating. Native hardware requests accept `port`;
specify a registered port when multiple instances are configured. The old
execute/collect/abort API and `--t32chat` no longer apply; use the upstream
`trace32_*` API and an explicit execution policy. See
[capabilities and safety limits](local-capabilities.md).

The client must be able to find uv's tool executable directory in `PATH`.
If necessary, obtain the absolute executable path using
`(Get-Command t32mcp).Source` and use that as the client's `command`.

## Build and install local artifacts

From the checkout root, with the same compiler environment:

```powershell
uv build --out-dir .\dist
```

This creates a source distribution and a platform-specific wheel. The source
distribution includes the Rust sources, Cargo lock file, native Remote API
libraries, and both license texts.

For a Windows x64 build of version `0.3.0`, install the resulting wheel:

```powershell
uv tool install --force --no-index .\dist\t32mcp-0.3.0-py3-none-win_amd64.whl
t32mcp --help
```

Use the actual filename produced by your build. A matching wheel does not need
Rust, MSVC, or libclang on the installation machine. Python still supplies uv's
isolated tool environment. TRACE32 and the external skills directory remain
separate runtime requirements.

To build wheel and source archives directly with Maturin:

```powershell
uvx --from "maturin>=1.9,<2" maturin build --release --sdist --out .\dist
```

For an isolated installation test without replacing an existing user tool, set
both `UV_TOOL_DIR` and `UV_TOOL_BIN_DIR` to temporary directories in the current
shell before running `uv tool install`. Clear or restore them afterwards.

The packaging checks require Python 3.11 or newer for `tomllib`, and run without
a board or third-party Python dependencies:

```powershell
python .\tests\test_packaging.py
```

Set `T32MCP_WHEEL` and `T32MCP_SDIST` to artifact paths to also verify the built
archives; otherwise those archive checks are skipped.

## Licenses

The server code is Apache-2.0 licensed. The linked TRACE32 Remote API has
separate licensing terms in `LICENSES/LicenseRef-Lauterbach-RemoteAPI.txt`.
Both license texts are included in the wheel and source distribution.
Review the Remote API terms before redistribution or commercial use; the
Apache license does not replace them.
