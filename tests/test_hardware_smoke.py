# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Offline smoke-probe tests: fake clients never launch a server or debugger."""

from types import SimpleNamespace

import hardware_smoke as probe
import pytest

TOOLS = {"get_debugger_status", "read_registers", "lookup_symbols"}


def batch(names):
    return {
        "count": len(names),
        "succeeded": len(names),
        "failed": 0,
        "results": [{"name": name, "ok": True, "value": 0} for name in names],
    }


def arguments(tmp_path, *extra):
    server = tmp_path / "fake-t32mcp.exe"
    server.write_bytes(b"offline fixture, never executed")
    return probe.parse_args(["--port", "1", "--server", str(server), *extra])


class FakeClient:
    def __init__(self, responses=None, tools=None):
        self.responses = responses or {}
        self.tools = TOOLS if tools is None else tools
        self.calls = []
        self.closed = False

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        self.closed = True

    async def list_tools(self):
        return [SimpleNamespace(name=name) for name in sorted(self.tools)]

    async def call_tool(self, name, args):
        assert name not in probe.FORBIDDEN_TOOLS
        self.calls.append((name, args))
        if name in self.responses:
            result = self.responses[name]
        elif name == "get_debugger_status":
            result = {"state": {"ok": True, "value": 3}}
        elif name in {"read_registers", "lookup_symbols"}:
            result = batch(args["names"])
        else:
            raise AssertionError(name)
        if isinstance(result, Exception):
            raise result
        return SimpleNamespace(structured_content=result, content=[])


async def fake_run(args, **kwargs):
    client = FakeClient(**kwargs)
    transports = []

    def factory(transport):
        transports.append(transport)
        return client

    report = await probe.run(args, client_factory=factory)
    assert transports[0].args == ["--native-tools", "--read-only", "--port", "1"]
    assert client.closed
    return report, client


def test_port_is_required_even_with_a_session_environment(monkeypatch):
    monkeypatch.setenv("T32_PORT", "1")
    with pytest.raises(SystemExit):
        probe.parse_args([])


@pytest.mark.parametrize("port", ["0", "-1", "65536", "not-a-port"])
def test_invalid_ports_are_rejected(port):
    with pytest.raises(SystemExit):
        probe.parse_args(["--port", port])


async def test_default_only_queries_status(tmp_path):
    args = arguments(tmp_path)
    assert args.register == []
    assert args.symbol == []
    report, client = await fake_run(args, tools={"get_debugger_status"})
    assert client.calls == [
        ("get_debugger_status", {}),
        ("get_debugger_status", {}),
    ]
    assert report["operations"] == ["get_debugger_status", "get_debugger_status"]
    assert report["registers"] is None
    assert report["symbols"] is None
    assert report["passed"] is True


async def test_arbitrary_registers_are_explicit_and_preserved(tmp_path):
    names = ["TARGET_REG_ALPHA", "TARGET_REG_BETA", "TARGET_REG_ALPHA"]
    extra = [option for name in names for option in ("--register", name)]
    report, client = await fake_run(arguments(tmp_path, *extra))
    assert client.calls == [
        ("get_debugger_status", {}),
        ("read_registers", {"names": names}),
        ("get_debugger_status", {}),
    ]
    assert report["registers"]["count"] == 3
    assert report["passed"] is True


async def test_symbols_do_not_implicitly_request_registers(tmp_path):
    names = ["symbol_alpha", "symbol_beta"]
    report, client = await fake_run(
        arguments(tmp_path, "--symbol", names[0], "--symbol", names[1])
    )
    assert client.calls == [
        ("get_debugger_status", {}),
        ("lookup_symbols", {"names": names}),
        ("get_debugger_status", {}),
    ]
    assert report["registers"] is None
    assert report["passed"] is True


@pytest.mark.parametrize(
    "response",
    [
        None,
        {},
        {"count": 1, "succeeded": 1, "failed": 0},
        {"count": 1, "succeeded": 1, "failed": 0, "results": []},
        {"count": 0, "succeeded": 0, "failed": 0, "results": []},
        {**batch(["REQUESTED"]), "succeeded": 0},
        {**batch(["REQUESTED"]), "count": 2},
        {**batch(["REQUESTED"]), "failed": 1},
        {**batch(["REQUESTED"]), "results": [{"name": "REQUESTED", "ok": False}]},
        {**batch(["REQUESTED"]), "results": [{"name": "REQUESTED"}]},
        {**batch(["REQUESTED"]), "results": [None]},
        batch(["OTHER"]),
        RuntimeError("native read failed"),
    ],
)
@pytest.mark.parametrize(
    "option,tool", [("--register", "read_registers"), ("--symbol", "lookup_symbols")]
)
async def test_requested_batch_must_be_complete_and_successful(
    tmp_path, option, tool, response
):
    report, client = await fake_run(
        arguments(tmp_path, option, "REQUESTED"), responses={tool: response}
    )
    assert report["passed"] is False
    assert client.calls[-1] == ("get_debugger_status", {})
    assert client.calls.count((tool, {"names": ["REQUESTED"]})) == 1


async def test_register_error_does_not_hide_unread_requested_symbols(tmp_path):
    report, client = await fake_run(
        arguments(tmp_path, "--register", "REG", "--symbol", "SYMBOL"),
        responses={"read_registers": RuntimeError("native read failed")},
    )
    assert report["passed"] is False
    assert report["symbols"] is None
    assert not any(name == "lookup_symbols" for name, _ in client.calls)
    assert client.calls[-1] == ("get_debugger_status", {})


async def test_unsafe_tool_exposure_stops_before_any_observations(tmp_path):
    client = FakeClient(tools=TOOLS | {"execute_practice"})
    with pytest.raises(RuntimeError, match="unsafe tools"):
        await probe.run(arguments(tmp_path), client_factory=lambda _: client)
    assert not client.calls
    assert client.closed
