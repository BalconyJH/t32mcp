# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Native instance selection is validated without contacting any PowerView."""

import asyncio
import json
import socket
import threading
from pathlib import Path

import pytest
from conftest import NATIVE_TOOLS, OFFLINE_NATIVE_TOOLS
from test_tools import result_object, tool_error
from test_workflows import assert_saved_artifact, snapshot_request

HARDWARE_TOOLS = sorted(NATIVE_TOOLS - OFFLINE_NATIVE_TOOLS)


@pytest.fixture
def connection_trap():
    """A test-owned TCP listener records unexpected connection attempts."""
    stopped = threading.Event()
    accepted = []
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(0.05)

        def drain():
            while not stopped.is_set():
                try:
                    connection, address = listener.accept()
                except socket.timeout:
                    continue
                with connection:
                    accepted.append(address)

        thread = threading.Thread(target=drain, daemon=True)
        thread.start()
        try:
            yield listener.getsockname()[1], accepted
        finally:
            stopped.set()
            thread.join(timeout=2)
            assert not thread.is_alive()


@pytest.fixture
def native_requests(tmp_path):
    image = tmp_path / "routing-fixture.bin"
    image.write_bytes(b"\x00")
    return {
        "doctor": {},
        "get_debugger_status": {},
        "evaluate_expressions": {"expressions": ["1."]},
        "read_registers": {"names": ["PC"]},
        "read_memory": {"regions": [{"address": "0x1000", "length": 1}]},
        "lookup_symbols": {"names": ["main"]},
        "read_debugger_window": {"window": "register"},
        "wait_for_condition": {"expression": "1.", "expected_value": "1."},
        "capture_snapshot": snapshot_request(),
        "verify_image": {
            "path": str(image),
            "format": "bin",
            "base_address": "0x1000",
            "allowed_ranges": [{"address": "0x1000", "length": 1}],
        },
        "read_per_registers": {
            "paths": ["RCC.CR"], "acknowledge_side_effects": True,
        },
        "read_per_views": {
            "views": [{"subtree": ["RCC"]}], "acknowledge_side_effects": True,
        },
    }


@pytest.mark.parametrize("tool_name", HARDWARE_TOOLS)
async def test_multiple_instances_require_explicit_port_before_any_probe(
    mcp_client_factory, connection_trap, native_requests, tool_name
):
    port, accepted = connection_trap
    async with mcp_client_factory("--native-tools", "--port", str(port)) as client:
        async with asyncio.timeout(10):
            error = await tool_error(client, tool_name, native_requests[tool_name])
    assert "explicit port" in error
    assert "TCP_PREFLIGHT_FAILED" not in error
    assert accepted == []


@pytest.mark.parametrize("tool_name", HARDWARE_TOOLS)
async def test_unregistered_port_is_rejected_without_connecting(
    mcp_client_factory, connection_trap, native_requests, tool_name
):
    port, accepted = connection_trap
    async with mcp_client_factory("--native-tools") as client:
        async with asyncio.timeout(10):
            error = await tool_error(
                client, tool_name, {**native_requests[tool_name], "port": port}
            )
    assert f"No instance with port {port} is added" in error
    assert accepted == []


@pytest.mark.parametrize("tool_name", HARDWARE_TOOLS)
async def test_zero_port_is_rejected_before_connecting(
    native_mcp_server, native_requests, tool_name
):
    async with native_mcp_server as client:
        error = await tool_error(
            client, tool_name, {**native_requests[tool_name], "port": 0}
        )
    assert "port" in error
    assert "127.0.0.1:" not in error


@pytest.mark.parametrize("port", [-1, 65536, "20000", 1.5, True])
async def test_invalid_native_port_types_and_ranges(native_mcp_server, port):
    async with native_mcp_server as client:
        error = await tool_error(client, "doctor", {"port": port})
    assert "port" in error
    assert "127.0.0.1:" not in error


async def test_explicit_native_port_selects_one_registered_instance(
    mcp_client_factory, offline_port, second_offline_port
):
    async with mcp_client_factory(
        "--native-tools", "--port", str(second_offline_port)
    ) as client:
        for port in (offline_port, second_offline_port):
            result = await client.call_tool("doctor", {"port": port})
            report = result_object(result)
            assert report["instance_port"] == port
            assert report["endpoint"]["port"] == port
            assert report["outcome"] == "connection_failed"
            assert report["queries"]["attempted"] == 0


async def test_null_native_port_uses_the_single_registered_instance(
    native_mcp_server, offline_port
):
    async with native_mcp_server as client:
        report = result_object(await client.call_tool("doctor", {"port": None}))
    assert report["instance_port"] == offline_port
    assert report["endpoint"]["port"] == offline_port


async def test_offline_tools_do_not_probe_or_need_instance_selection(
    mcp_client_factory, tmp_path, connection_trap
):
    directory = tmp_path / "artifacts"
    async with mcp_client_factory(
        "--native-tools", "--artifacts", str(directory)
    ) as client:
        snapshots = [
            result_object(
                await client.call_tool(
                    "capture_snapshot", snapshot_request(save_artifact=True)
                )
            )
            for _ in range(2)
        ]
    before_id, after_id = [
        assert_saved_artifact(snapshot, directory, "snapshot")
        for snapshot in snapshots
    ]
    port, accepted = connection_trap
    async with mcp_client_factory(
        "--native-tools", "--port", str(port), "--artifacts", str(directory)
    ) as client:
        catalog = result_object(await client.call_tool("list_practice_skills", {}))
        difference = result_object(
            await client.call_tool(
                "diff_snapshots", {"before_id": before_id, "after_id": after_id}
            )
        )
    assert catalog["skills"]
    assert difference["format"] == "t32mcp.snapshot_diff"
    assert difference["before_id"] == before_id
    assert difference["after_id"] == after_id
    assert difference["instance_comparison"]["status"] == "matching_recorded_port"
    assert accepted == []


async def test_saved_snapshot_instance_identity_is_not_silently_ignored(
    mcp_client_factory, tmp_path, offline_port, second_offline_port
):
    directory = tmp_path / "identity-artifacts"
    async with mcp_client_factory(
        "--native-tools", "--artifacts", str(directory)
    ) as client:
        snapshots = [
            result_object(
                await client.call_tool(
                    "capture_snapshot", snapshot_request(save_artifact=True)
                )
            )
            for _ in range(2)
        ]
        ids = [
            assert_saved_artifact(snapshot, directory, "snapshot")
            for snapshot in snapshots
        ]
        after_path = Path(snapshots[1]["artifact"]["path"])
        after = json.loads(after_path.read_text(encoding="utf-8"))
        assert after["payload"]["instance_port"] == offline_port
        arguments = {"before_id": ids[0], "after_id": ids[1]}

        after["payload"]["instance_port"] = second_offline_port
        after_path.write_text(json.dumps(after), encoding="utf-8")
        error = await tool_error(client, "diff_snapshots", arguments)
        assert "different PowerView instance ports" in error

        del after["payload"]["instance_port"]
        after_path.write_text(json.dumps(after), encoding="utf-8")
        legacy = result_object(await client.call_tool("diff_snapshots", arguments))
        assert legacy["instance_comparison"]["status"] == "unknown"
        assert "legacy snapshot" in legacy["instance_comparison"]["reason"]


@pytest.mark.parametrize("tool_name", sorted(OFFLINE_NATIVE_TOOLS))
async def test_offline_tools_reject_irrelevant_port(native_mcp_server, tool_name):
    async with native_mcp_server as client:
        error = await tool_error(client, tool_name, {"port": 1})
    assert "port" in error


async def test_skill_discovery_supports_multiple_new_style_roots(
    mcp_client_factory, tmp_path, skills_base
):
    second = tmp_path / "more-skills"
    skill = second / "lauterbach-trace32-skill-creator"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("# Documentation only\n", encoding="ascii")
    async with mcp_client_factory(
        "--native-tools", "--skills", str(second)
    ) as client:
        report = result_object(await client.call_tool("list_practice_skills", {}))
    roots = [Path(path) for path in report["skills_roots"]]
    assert len(roots) == 2
    assert all(any(root.samefile(path) for root in roots) for path in (skills_base, second))
    skills = {item["skill_name"]: item for item in report["skills"]}
    assert set(skills) == {"fixture", "lauterbach-trace32-skill-creator"}
    assert skills["lauterbach-trace32-skill-creator"]["scripts"] == []
    assert Path(skills["fixture"]["skills_base"]).samefile(skills_base)
    assert Path(skills["lauterbach-trace32-skill-creator"]["skills_base"]).samefile(second)


async def test_catalog_names_roundtrip_to_strict_resolver_without_prefix_collisions(
    mcp_client_factory, tmp_path, connection_trap
):
    root = tmp_path / "colliding-skills"
    required_arguments = {"foo": "plain_only", "skill-foo": "legacy_only"}
    for name, argument in required_arguments.items():
        skill = root / name
        (skill / "scripts").mkdir(parents=True)
        (skill / "SKILL.md").write_text(f"# {name}\n", encoding="ascii")
        header = {
            "interface": 1, "strict": True,
            "arguments": {argument: {"type": "symbol"}},
        }
        (skill / "scripts" / "read.cmm").write_text(
            f"; {json.dumps(header)}\n\nENDDO\n", encoding="ascii"
        )
    port, accepted = connection_trap
    async with mcp_client_factory(
        "--native-tools", "--execution-policy", "strict",
        "--skills", str(root), "--port", str(port),
    ) as client:
        report = result_object(await client.call_tool("list_practice_skills", {}))
        catalog = {item["skill_name"]: item for item in report["skills"]}
        assert set(required_arguments) <= set(catalog)
        assert catalog["skill-foo"]["legacy_name"] == "foo"
        for name, argument in required_arguments.items():
            error = await tool_error(
                client,
                "trace32_run_script",
                {
                    "skill_name": catalog[name]["skill_name"],
                    "script_name": catalog[name]["scripts"][0],
                },
            )
            assert f"Missing required argument '{argument}'" in error
            assert "127.0.0.1:" not in error
    assert accepted == []
