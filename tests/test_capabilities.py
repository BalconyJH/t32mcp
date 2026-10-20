# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Target capability policy tests; fixtures reserve unlistened TCP ports."""

import json
import subprocess

import pytest
from test_peripherals import registers_request, views_request
from test_tools import result_object, tool_error


def target_config(tmp_path, policy):
    path = tmp_path / "target.json"
    path.write_text(json.dumps({"per_support": policy}), encoding="utf-8")
    return str(path)


@pytest.mark.parametrize(
    ("policy", "status", "source"),
    [
        ("auto", "unknown", "auto"),
        ("supported", "user_declared_supported", "user_config"),
        ("unsupported", "user_declared_unsupported", "user_config"),
    ],
)
async def test_doctor_preserves_policy_without_claiming_an_observation(
    mcp_client_factory, tmp_path, policy, status, source
):
    config = target_config(tmp_path, policy)
    async with mcp_client_factory(
        "--native-tools", "--target-config", config
    ) as client:
        report = result_object(await client.call_tool("doctor", {}))
    assert report["outcome"] == "connection_failed"
    assert report["queries"]["attempted"] == 0
    per = report["capabilities"]["per"]
    assert per["status"] == status
    assert per["source"] == source
    assert per["active_definition"]["status"] == "not_attempted"


@pytest.mark.parametrize(
    ("tool", "arguments", "stop_code"),
    [
        ("read_per_registers", registers_request(), "CONFIGURED_UNSUPPORTED"),
        ("read_per_views", views_request(), "configured_unsupported"),
    ],
)
async def test_unsupported_policy_refuses_per_before_connection(
    mcp_client_factory, tmp_path, tool, arguments, stop_code
):
    config = target_config(tmp_path, "unsupported")
    async with mcp_client_factory(
        "--native-tools", "--read-only", "--target-config", config
    ) as client:
        names = {entry.name for entry in await client.list_tools()}
        result = await client.call_tool(tool, arguments, raise_on_error=False)
    assert tool in names
    assert not result.is_error
    report = result_object(result)
    assert report["ok"] is False
    assert report["stop_reason"]["code"] == stop_code
    assert len(report["results"]) == 2
    assert all(item["status"] == "not_attempted" for item in report["results"])
    assert "127.0.0.1:" not in json.dumps(report)
    if tool == "read_per_registers":
        assert report["counts"]["evaluations"] == 0
        assert report["counts"]["state_probes"] == 0
    else:
        assert report["summary"]["attempted"] == 0
        assert report["target_state"]["checks"] == 0


@pytest.mark.parametrize("policy", ["auto", "supported", "unsupported"])
@pytest.mark.parametrize(
    ("tool", "arguments"),
    [
        (
            "read_per_registers",
            registers_request(acknowledge_side_effects=False),
        ),
        ("read_per_views", views_request(acknowledge_side_effects=False)),
    ],
)
async def test_policy_never_bypasses_side_effect_acknowledgement(
    mcp_client_factory, tmp_path, policy, tool, arguments
):
    config = target_config(tmp_path, policy)
    async with mcp_client_factory(
        "--native-tools", "--target-config", config
    ) as client:
        error = await tool_error(client, tool, arguments)
    assert "acknowledge_side_effects" in error
    assert "127.0.0.1:" not in error


@pytest.mark.parametrize(
    "contents",
    [
        "",
        "not json",
        '{"per_support":"guess"}',
        '{"per_support":true}',
        '{"per_support":"auto","cpu":"TestBoard"}',
    ],
)
def test_invalid_config_fails_at_startup(
    server_executable, tmp_path, contents, offline_port
):
    path = tmp_path / "invalid.json"
    path.write_text(contents, encoding="utf-8")
    result = subprocess.run(
        [
            server_executable, "--execution-policy", "scripts",
            "--port", str(offline_port), "--target-config", str(path),
        ],
        input="",
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    assert result.returncode != 0
    assert "127.0.0.1:" not in result.stderr
