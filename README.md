# Lauterbach TRACE32 MCP Support (Preview)

> [!NOTE]  
> This is an early development preview. The implementation is still evolving and may change in future releases.

[![asciicast](https://asciinema.org/a/BSZGeOKfzWOMpVFN.svg)](https://asciinema.org/a/BSZGeOKfzWOMpVFN)

**Questions & Feedback**:
We would love to get your feedback and improvement suggestions for our MCP implementation. For questions, feedback and help, please refer to the [contribution guidelines](./CONTRIBUTING.md).

## Table of Contents
1. [Files overview](#files-overview)
2. [MCP server](#mcp-server)
3. [Agent skills](#agent-skills)
4. [AI agent configuration](#ai-agent-configuration)
5. [Intended agent behavior](#intended-agent-behavior)
6. [Concept and AI roadmap](#concept-and-ai-roadmap)

## Files overview

```
t32mcp/                                # this repository
├── README.md                          # this file
├── t32mcp-rs/                         # the MCP server source code
├── CLAUDE.md                          # debugging instructions example
└── .claude/
    └── skills/
        └── skill-trace32/             # Accompanying Agent Skill
            ├── SKILL.md               # Script reference and debugging workflow
            ├── references/            # Optional native, PER and evidence guides
            └── scripts/
                ├── <script-name>.cmm  # the "actual tools" implemented as PRACTICE scripts
                └── ...
```

## MCP server

### Local enhancements and uv installation

This local checkout preserves the three-tool PRACTICE interface by default and
adds native-binary packaging for `uv tool`:

```powershell
uv tool install .
t32mcp --skills "C:\path\to\skills"
```

Use your installed skills root. The server uses port `20000` unless `--port`
selects the port configured in your own PowerView session.

Source installation requires Rust, a native linker, and libclang. Installing a
locally built wheel does not require a compiler. See
[local installation](docs/installation.md) for prerequisites and wheel commands,
and [local capabilities](docs/local-capabilities.md) for tool schemas, safety
limits and verification. These are local changes; no package publication or
remote release is required.

Structured inspection, batch PER reads, snapshots and image readback checks
remain available through the explicit `--native-tools` option. They are a local
extension, not a change to the contribution policy: new upstream capabilities
should normally be PRACTICE scripts documented in `SKILL.md`.

An advertised tool is a server interface, not proof that the connected chip
supports it. PER requires chip/session support and a compatible active
definition; register names, memory maps, access modes and debugger windows also
depend on the target configuration. No test-board CPU, PER file, address map or
debug-port setup is installed or selected by default.

Optional `--target-config FILE` accepts a session-specific JSON declaration,
such as `{"per_support":"unsupported"}`. Otherwise `doctor` uses conservative
metadata detection, not register reads or a CPU compatibility table. See
[capability configuration](docs/local-capabilities.md#session-capability-configuration).

You can either download and use one of the prebuilt executables (if compatible) or build it yourself from source.
For simplicity the rest of this README assumes the executable sits at `<your-path>/t32mcp/t32mcp` / `<your-path>\t32mcp\t32mcp.exe`.

**Note:** Currently this MCP setup only works when the MCP server and TRACE32 are running on the same system. Specifically:
- the path to the MCP server executable needs to be accessible by the MCP client (your AI agent, e.g. Claude Code)
- the MCP server executable needs to be able to communicate with TRACE32 via:
  1. the remote API (TCP by default)
  2. IPC pipes (used in order to get the results from TRACE32 back to the MCP server)

### Download a prebuilt release (recommended)

Prebuilt executables for x86-64 Linux and Windows are attached to each release.
Grab them from the [releases page](https://gitlab.com/lauterbach/t32mcp/-/releases):
- `t32mcp-linux-x86_64`
- `t32mcp-windows-x86_64.exe`
- `skill-trace32.zip` for the accompanying TRACE32 agent skill (`skill-trace32/` with `SKILL.md` and the PRACTICE scripts)

Download the executable for your platform and the skill zip, then unzip `skill-trace32.zip` to wherever you keep your skills (see [Skills](#skills)). On Linux make the executable runnable with `chmod +x t32mcp-linux-x86_64`. You may rename and move the executable to your preferred location.

### Build it yourself

The Rust source code for the MCP server is provided in the `t32mcp-rs` directory. Use `cargo build --release` in that directory to get the portable executable `t32mcp` (Linux) / `t32mcp.exe` (Windows) (in `t32mcp/t32mcp-rs/target/release/`). You may move it to your preferred location.

### Usage

The MCP server communicates with the MCP client via the `stdio` protocol.

The executable should be called with the command-line parameter `--skills <path-to-skills>`. This points the MCP server to the root directory from which it loads the PRACTICE scripts `<path-to-skills>/skill-trace32/scripts/*.cmm`.

The optional `--port` parameter defaults to 20000 (the default port for the
TRACE32 Remote API). If you use a different port, provide it here. Use `--help`
for transport and local extension options.

### Tools

The default interface contains only the three generic PRACTICE tools:

1. `execute_practice_skill`: to execute a script
2. `abort_practice_skill`: to cancel the current script
3. `collect_practice_skill_response`: to get / poll the results of a running script

Local extension modes are explicit:

| Options | Tools exposed |
| --- | --- |
| Default | 3 PRACTICE tools |
| `--t32chat` | 4: PRACTICE tools and unrestricted `execute_practice` |
| `--native-tools` | 17: PRACTICE tools and 14 native extensions |
| `--native-tools --t32chat` | 18 |
| `--read-only` | 1: `collect_practice_skill_response` |
| `--native-tools --read-only` | 15: collection and native extensions |

`--read-only` suppresses execution and abort, including `execute_practice` when
`--t32chat` is also supplied. To keep using native tools after updating an older
local installation, add `--native-tools` to the MCP client's server arguments.
The native tool names and schemas remain available in that opt-in mode.

The 14 native extensions are:

| Tool | Purpose |
| --- | --- |
| `doctor` | Structured endpoint/attachment/status diagnostics, including unavailable sessions |
| `capture_snapshot` | Named observations with independent expectations and optional local evidence |
| `diff_snapshots` | Offline comparison of saved snapshots, preserving unavailable/incompatible results |
| `verify_image` | Bounded BIN/Intel HEX readback checks on an already halted/stopped target |
| `get_debugger_status` | Target/PRACTICE state, CPU and software version |
| `evaluate_expressions` | Ordered batch queries with native result types and individual errors |
| `read_registers` | Batch register reads with full-width hexadecimal values |
| `read_per_registers` | Up to 64 PER register/bitfield paths with independent native results |
| `read_per_views` | Up to 16 selected PER subtrees with byte-based pagination |
| `read_memory` | Bounded memory regions, hexdump and optional expected-byte comparison |
| `lookup_symbols` | Exact loaded-symbol address, size and reserved native metadata |
| `read_debugger_window` | Paginated register, frame, locals, breakpoint or task text |
| `wait_for_condition` | Deadline-based observation with samples, attempts and elapsed time |
| `list_practice_skills` | Discover script names without reading or running them |

`--read-only` is not a hardware sandbox:
MMIO reads, target-specific expression functions and executable PER definitions
can have side effects, including writes from PER logic.
Nothing connects to the target until a tool actually needs the Remote API.
Inspection tools do not require a skills directory; skill discovery/execution
do.

See [evidence workflows](docs/evidence-workflows.md) for the new tools. Evidence
persistence is opt-in (`save_artifact=true`) and `--artifacts DIR` selects its
local directory. Existing evidence is never overwritten or automatically deleted.

See [batch PER inspection](docs/peripheral-inspection.md) for multi-register and
multi-subtree examples. Both PER tools require explicit side-effect
acknowledgement and an already halted/stopped target; they never auto-halt or
modify the shared Watch list. Neither native extensions nor `--t32chat` are
required for the default script workflow.

## Agent skills

The skill documents the default PRACTICE workflow and every script's purpose,
arguments and examples directly in `SKILL.md`. Its `references/` directory
contains optional native-inspection, batch-PER and evidence-workflow guides,
used only when the connected server advertises those tools. Copy the complete
skill folder, not just `SKILL.md`.

### Scripts

The skill's `scripts` directory contains a collection of PRACTICE (`*.cmm`) scripts that provide the actual tool capabilities for the AI agent.

Scripts can take input arguments and can return information to the AI agent by printing to the `AREA` window.
Take the very simple `sw_get_stack_frame.cmm` as an example:
```cmm
PRinTer.Area
WinPrint.Frame

ENDDO
```

**Note:** Document every script directly in `SKILL.md`; keep script changes and
their documentation consistent, as required by [CONTRIBUTING.md](CONTRIBUTING.md).

## AI agent configuration

### MCP

To add our MCP server, follow the instructions for your AI agent. Usually, this includes specifying the protocol, which should be set to `stdio` (sometimes also `local`).
Then set the command / path to the executable `<your-path>/t32mcp/t32mcp` / `<your-path>\t32mcp\t32mcp.exe`.

**Important:** also add the command argument `--skills <path-to-your-skills>`. What to set for `<path-to-your-skills>` will depend on how you configured your skills (next section).

### Skills

Most AI agents support skills from multiple standard directories (both global and local / project-based), e.g. in `.claude/skills/`.

Place our `skill-trace32` skill into one of those supported skill directories (referenced here as `<path-to-your-skills>`). Either unzip the released `skill-trace32.zip` there, or copy it from a checkout at `<your-path>/t32mcp/.claude/skills/skill-trace32`. Then use that directory `<path-to-your-skills>` for the `--skills` argument when adding the MCP server!

**Important:** If you customize the skill and scripts, e.g. by adding new scripts, **you must point the MCP to the correct skill directory with the `--skills` argument**. If you instead point the MCP to the unmodified original git checkout, the AI agent will think it has access to the new functionality, however, the MCP server will not be able to find the corresponding PRACTICE scripts!

**Note:** If you have copied `skill-trace32` to both local and global skill directories (and potentially even edited them so they differ), make sure you understand your AI agent's priority order that determines which skill is actually loaded.

#### Customization

In your local project you can freely edit `skill-trace32/SKILL.md` and the corresponding PRACTICE scripts to customize the AI agent's TRACE32 interface and instructions, without affecting other projects or the original configuration. You could also add new skills such as `skill-trace32-arm-etm-trace`. If you already have a library of useful PRACTICE scripts, you can also make these available to the AI agent through skills.

### Claude Code example

This registers the MCP server with Claude Code. It assumes you have already placed `skill-trace32` into a skills directory (see [Skills](#skills)). Substitute your own two paths:
- the MCP server executable
- the skills directory that contains `skill-trace32`

The command is the same on every platform; only the path syntax differs.

Linux example:
```bash
claude mcp add-json t32mcp '{"type":"stdio","command":"/home/you/t32mcp/t32mcp","args":["--skills", "/home/you/my-project/.claude/skills"]}'
```

Windows (PowerShell) example:
```powershell
claude mcp add-json t32mcp '{"type":"stdio","command":"C:\\Users\\you\\t32mcp\\t32mcp.exe","args":["--skills", "C:\\Users\\you\\my-project\\.claude\\skills"]}'
```

(Optional) Use our "debug instructions" by copying the provided `CLAUDE.md` into your project root / into your own `CLAUDE.md`. Note that we have not evaluated how well these work, feel free to change.

Note: to remove the MCP server from Claude Code again, use `claude mcp remove t32mcp`.


## Intended agent behavior

If you ask your agent to use TRACE32 to set a register value on your target, it
should read the `trace32` skill. For this authorized mutation, `SKILL.md`
contains:

````md
#### `core_set_register.cmm`

Set core register `register` to value `value`.

```json
{
  "type": "object",
  "properties": {
    "register": { "type": "string" },
    "value": {"type": "string"}
  },
  "required": ["register", "value"]
}
```

Example:

```json
{
    "register": "x1",
    "value": "0x10"
}
```
````

It should then check the tools provided by the MCP server and should decide to use the `execute_practice_skill` tool to execute the `core_set_register.cmm` script with the corresponding parameters.

## Concept and AI roadmap

Ideally, an AI agent would be able to use TRACE32 in the same way as a human (expert).
Conveniently for LLMs, TRACE32 can be fully controlled via text commands (PRACTICE / `cmm` scripts).
Unfortunately, there are a lot of PRACTICE commands and general-purpose LLMs do not seem to know much about them. This leads to very high hallucination rates for TRACE32-related information (especially when not using web-search tools).

A common way to mitigate this is to directly provide the TRACE32 manuals to the AI agent.
Indeed, we have invested some effort in providing information and additional tools to make the AI a more useful TRACE32 assistant. The TRACE32 AI Assistant is available as a chatbot on the website [https://assistant.ai.lauterbach.com](https://assistant.ai.lauterbach.com) (limited preview, please sign up with your company email address to increase the likelihood of getting access).

We also plan to make the chatbot functionality available as a remote MCP server, such that your AI agent can benefit from those same informed TRACE32 answers and resources.
Once that is available, we could also try allowing the AI agent to interface with TRACE32 directly via PRACTICE (as a human expert would), truly "unlocking" all TRACE32 features for the AI to use.

But until then we need some sort of "wrapper interface" to TRACE32 that the AI can understand. For now we opted for the "MCP + Agent skill + PRACTICE scripts" approach described above, since it also allows for user customization and extensions while allowing the AI to access virtually any TRACE32 functionality (once provided as a script).
In the meantime **you can have our TRACE32 AI Assistant help you write your own skills and extension PRACTICE scripts!**


# License

The server code is licensed under the Apache License, Version 2.0; see
[Apache-2.0](LICENSES/Apache-2.0.txt). The linked TRACE32 Remote API has
[separate licensing terms](LICENSES/LicenseRef-Lauterbach-RemoteAPI.txt).
