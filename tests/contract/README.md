# Script interface contract tests

t32mcp supports a single script interface version (`SCRIPT_INTERFACE_VERSION`
in `t32mcp-rs/src/execution_policy.rs`). Skill scripts declare theirs with `"interface": <n>`
in the header JSON object.

`interface-<n>/contract/` is a skill with fixture scripts written against interface `<n>`, the way the
skill scripts of that time use it: arguments via `ENTRY %LINE` and `STRing.SCANAndExtract`, output via
the AREA, errors via `ON ERROR`.

- Never edit the fixtures of a released interface version. A t32mcp change that makes them fail
  breaks the scripts of that version: bump `SCRIPT_INTERFACE_VERSION`, add `interface-<n+1>/`,
  and bump the major version of the skills.
- These tests run scripts in PowerView and are never enabled merely because a
  debugger is reachable. Set `T32MCP_HARDWARE=1` and `T32MCP_T32_PORT` to the
  explicitly approved session port, then run `uv run pytest test_contract.py`
  in `tests/`. The same opt-in gates `test_device_prompt.py`.
- Without both variables the tests are skipped without probing a port, even
  during collection, or starting a simulator. `T32SYS` alone never enables
  them. There is no default port; invalid ports fail before I/O.
- After explicit opt-in, the fixture reuses the selected PowerView. If it is
  unreachable and `T32SYS` is set, it starts a headless RISC-V Instruction Set
  Simulator from that installation using `tests/sim.t32` and closes only that
  owned process afterwards. Without either a reachable session or `T32SYS`,
  the tests skip. See [test setup](../README.md).
- Use a disposable simulator/session: these tests execute PRACTICE, create
  windows, and change the device prompt.
