# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] - Local Enhancements
### Changed
- Merge upstream 0.3.0: require an explicit execution policy, use the new
  `trace32_*` tools and skill layout, and retain optional native inspection on
  the shared worker. Native requests accept an optional registered `port`;
  multiple instances require an explicit selection.
- Keep local uv packaging and the vendor API used by native inspection.
- Replace `--t32chat` with `--execution-policy unrestricted`. `--read-only`
  now exposes status only, plus the 14 native tools when explicitly enabled.
- Keep chip/session capabilities separate from advertised MCP tools. Passive
  diagnostics preserve unknown support and user declarations separately from
  definition metadata; general setup examples do not reuse a test board's
  endpoint or paths. Hardware smoke register reads are now explicitly selected.
- Preserve the 14 local native extensions behind explicit `--native-tools`.
  Existing native-tool clients must add that option when updating.
- Keep optional native guidance separate from the upstream script workflow.
- Separate MCP routing from feature implementations and simplify PER batch
  control flow without changing the native request/response contracts.

### Added
- Optional `--target-config` declarations for PER support and conservative
  `PER.FILENAME()` metadata detection without register reads, file loading or
  CPU compatibility tables.
- Batch PER register/bitfield queries (up to 64 paths) and selected subtree
  pages (up to 16), with explicit side-effect acknowledgement, stopped-state
  guards, independent errors and native byte continuation offsets.
- Named snapshots with independent expectations, offline snapshot diffs, and
  opt-in bounded local evidence files selected by `--artifacts`.
- BIN/Intel HEX readback verification with explicit allowed address ranges,
  image hashes, sparse coverage and partial-result semantics.
- Passive `doctor` reports with structured connection/native errors and
  evidence-based capabilities, without automatic recovery.
- Native-binary packaging for local `uv tool install` and local wheels.
- Structured status, batched expressions/registers/memory/symbols, paginated
  debugger windows, conditional observation and installed-skill discovery.
- Expected-byte memory comparison and bounded polling history.
- `--read-only` execution-tool restriction and explicit hardware smoke test.
- Checkpointed, opt-in hardware inspection with explicit safe memory ranges.
- Hardware-free validation, protocol, packaging and input-boundary tests.

### Fixed
- Script path containment, input validation, stale output readers and unbounded
  response buffers.
- Native string decoding, reconnect channel cleanup and CLI error reporting.
- Window pagination continues after short pages and stops only on empty reads.
- Memory inspection uses documented address/buffer objects and rejects
  potentially intrusive running-target access instead of using StopAndGo.
- Missing symbols reported by the native address/size `0xFFFFFFFF` sentinel now produce
  per-item errors rather than fabricated successful addresses.
- Documented the distinction between asynchronous polling limits and blocking
  vendor TCP API calls.

## [0.3.0] - 2026-10-19

**Note to existing users**:
To provide better control over your agent, this version of `t32mcp` requires you to explicitly set an `--execution-policy`.
To use the `strict` policy, you will also need to migrate the headers of your existing scripts to the schema which is described in [PRACTICE Script Authoring](./README.md).
To simplify migration, we are providing you also with a new skill `lauterbach-trace32-skill-creator` which describes this schema in detail.

### Added
- AMP support: `--port` is repeatable. PowerView instances are addressed by their Remote API port, cores by their core number; `trace32_manage_instances` can manage instances at runtime.
- Execution policy support (`--execution-policy`): `unrestricted`, `scripts`, `strict`. `strict` only executes skill scripts that declare `"strict": true` in their header.
- HTTP transport options: `--bind` (default `127.0.0.1`), `--token`, `--allow-origin`. Binding a non-loopback address requires token and allowed origin.
- Skill `lauterbach-trace32-skill-creator`: Create new skills with the help of `t32chat`.

### Changed
- **Breaking**: Tool set replaced with `trace32_manage_instances`, `trace32_get_status`, `trace32_run_script`, `trace32_run_command` (previous tool set: [0.1.0](#010---2026-06-30)).
- Renamed `skill-trace32` to `lauterbach-trace32-debug-basic`.
- Multiple `--skills` directories can be specified.
- Dropped `rmcp` dependency: MCP implemented using `typify` and `serde` instead. Server runs multi-threaded.

### Fixed
- Improved named pipe connection: lazily initialized and persistent.
- Hardening against script injections: PRACTICE script header, see "PRACTICE Script Authoring" in the README

## [0.2.2] - 2026-07-31
### Changed
- Bumped version for CI.
- Fixed CI build of linux arm target.

## [0.2.1] - 2026-07-30
### Added
- Prebuilt `t32api64` library for `aarch64-unknown-linux-gnu`, enabling the
  Linux ARM64 build (native or cross-compiled) alongside x86_64 and Windows.
- CI now builds and releases `t32mcp-linux-aarch64`.
- Specifiy licenses with [REUSE](https://reuse.software/).
- Add contributing guidelines.

### Changed
- `t32mcp-rs/lib` is now organized into per-target subdirectories
  (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-pc-windows-msvc`) since the Linux x86_64 and aarch64 `t32api64`
  libraries share the same filename.
- Update cargo dependencies.

## [0.2.0] - 2026-07-15
### Added
- Support HTTP transport layer.
- Update provided server info: t32mcp will not longer be displayed as `rmcp` in clients like VSCode.
- Add `--version` flag.
- Add pytest-based integration test suite which is also added to CI.

### Changed
- Format of `--help` output: Optional arguments are no longer displayed on first line.
- Change all `CRLF` line terminators to `LF`.

## [0.1.0] - 2026-06-30
### Added
- Initial release of the TRACE32 MCP server.
- Rust implementation that connects to TRACE32 via the remote API.
- Tools: `execute_practice_skill`, `collect_practice_skill_response`, `abort_practice_skill`.
- Includes basic skills for standard debugging operations, with each operation provided by a PRACTICE script.
- Prebuilt executables for x86-64 Linux (`t32mcp-linux-x86_64`) and Windows
  (`t32mcp-windows-x86_64.exe`), plus the TRACE32 skill bundled as
  `skill-trace32.zip`, built in CI and attached to the release, so building
  from source is optional.
