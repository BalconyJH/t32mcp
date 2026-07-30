# Contributing to t32mcp

Thank you for your interest in this project. We welcome contributions via GitLab, both as issues and as merge requests.

## Reporting Issues

Please use [GitLab work items](https://gitlab.com/lauterbach/t32mcp/-/issues) to report:
- **Skill issues**: problems with `SKILL.md` or the PRACTICE scripts in `.claude/skills/`.
- **MCP server issues**: bugs or unexpected behavior in the `t32mcp` server (`t32mcp-rs/`).

When filing an issue, please include reproduction steps, your platform, and the relevant TRACE32/MCP client versions.

## Submitting Changes

Please submit changes as [GitLab merge requests](https://gitlab.com/lauterbach/t32mcp/-/merge_requests). We accept:
- **Skill additions**: new or improved PRACTICE scripts, together with corresponding updates to `SKILL.md`. Every script must be documented there.
- **MCP server fixes**: bug fixes and improvements to the existing server implementation.

### A note on new MCP tools

We follow a **progressive disclosure** approach: the MCP server intentionally exposes only three generic tools (`execute_practice_skill`, `abort_practice_skill`, `collect_practice_skill_response`), and new capabilities are meant to be added as PRACTICE scripts in the skill rather than as new MCP tools. We are therefore hesitant to accept merge requests that add new tools to the server. If you believe a new tool is genuinely necessary, please open an issue to discuss the rationale before submitting a merge request.
