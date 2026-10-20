# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Offline workflow regressions: fixtures reserve unlistened TCP ports."""

import hashlib
import json
from pathlib import Path

import pytest
from test_tools import result_object, tool_error

WORKFLOW_TOOLS = {"doctor", "capture_snapshot", "diff_snapshots", "verify_image"}


def snapshot_request(*, save_artifact=False):
    return {
        "items": [
            {"name": "target", "query": {"kind": "status", "expected_state": 3}},
            {
                "name": "constant",
                "query": {
                    "kind": "expression",
                    "expression": "1",
                    "expected_value": "1.",
                },
            },
        ],
        "save_artifact": save_artifact,
    }


def bin_request(path: Path):
    return {
        "path": str(path),
        "format": "bin",
        "base_address": "0x1000",
        "allowed_ranges": [{"address": "0x1000", "length": 4}],
    }


def hex_record(address: int, data: bytes, kind=0):
    record = bytes([len(data), address >> 8, address & 0xFF, kind]) + data
    return ":" + (record + bytes([-sum(record) & 0xFF])).hex().upper()


def assert_validation_preceded_connection(error: str):
    assert "127.0.0.1:" not in error, error
    assert "RCL=NETTCP" not in error, error
    assert "TCP_PREFLIGHT_FAILED" not in error, error


def assert_saved_artifact(response, directory: Path, kind: str):
    artifact = response["artifact"]
    assert artifact["saved"] is True
    assert artifact["kind"] == kind
    assert len(artifact["id"]) == 32
    assert all(character in "0123456789abcdef" for character in artifact["id"])
    path = Path(artifact["path"])
    assert path.resolve().parent == directory.resolve()
    assert path.name == f"{artifact['id']}.json"
    raw = path.read_bytes()
    assert artifact["bytes"] == len(raw)
    assert artifact["sha256"] == hashlib.sha256(raw).hexdigest()
    envelope = json.loads(raw)
    assert envelope["format"] == "t32mcp.artifact"
    assert envelope["version"] == 1
    assert envelope["id"] == artifact["id"]
    assert envelope["kind"] == kind
    assert isinstance(envelope["created_unix_ms"], int)
    assert envelope["server_version"]
    assert envelope["payload"] == {
        key: value for key, value in response.items() if key != "artifact"
    }
    return artifact["id"]


async def test_workflow_tools_have_object_schemas(native_mcp_server):
    async with native_mcp_server as client:
        tools = {tool.name: tool for tool in await client.list_tools()}
    assert WORKFLOW_TOOLS <= set(tools)
    for name in WORKFLOW_TOOLS:
        assert tools[name].inputSchema["type"] == "object"
    assert set(tools["capture_snapshot"].inputSchema.get("required", [])) == {"items"}
    assert {"items", "save_artifact"} <= set(
        tools["capture_snapshot"].inputSchema["properties"]
    )
    assert set(tools["diff_snapshots"].inputSchema.get("required", [])) == {
        "before_id",
        "after_id",
    }
    assert set(tools["verify_image"].inputSchema.get("required", [])) == {
        "path",
        "format",
        "allowed_ranges",
    }
    for name in ("capture_snapshot", "diff_snapshots", "verify_image"):
        assert tools[name].inputSchema.get("additionalProperties") is False


async def test_workflow_tools_remain_available_read_only(mcp_client_factory):
    async with mcp_client_factory("--native-tools", "--read-only") as client:
        tools = {tool.name for tool in await client.list_tools()}
        result = await client.call_tool("doctor", {}, raise_on_error=False)
    assert WORKFLOW_TOOLS <= tools
    assert not result.is_error
    assert result_object(result)["outcome"] == "connection_failed"


async def test_doctor_absent_endpoint_returns_evidence_not_mcp_error(
    native_mcp_server, offline_port,
):
    async with native_mcp_server as client:
        result = await client.call_tool("doctor", {}, raise_on_error=False)
    assert not result.is_error
    report = result_object(result)
    assert report["schema_version"] == 1
    assert report["endpoint"] == {
        "transport": "tcp",
        "host": "127.0.0.1",
        "port": offline_port,
    }
    assert report["outcome"] == "connection_failed"
    assert report["connection"]["ok"] is False
    error = report["connection"]["error"]
    assert error["code"] == "TCP_PREFLIGHT_FAILED"
    assert error["stage"] == "tcp_preflight"
    assert error["operation"] == "tcp_connect"
    assert "native_code" not in error
    assert report["target"]["state"] == "unknown"
    assert "native_state" not in report["target"]
    assert report["queries"] == {"attempted": 0, "succeeded": 0, "failed": 0}
    assert report["errors"] == [error]
    for observation in report["observations"].values():
        assert observation["status"] == "not_attempted"
        assert observation["reason"] == "connection_failed"
    assert all(
        capability["status"] == "unknown"
        for capability in report["capabilities"].values()
    )
    assert "per" in report["capabilities"]


@pytest.mark.parametrize(
    ("arguments", "expected"),
    [
        ({"items": []}, "between 1 and 64"),
        (
            {
                "items": [
                    {"name": f"s{index}", "query": {"kind": "status"}}
                    for index in range(65)
                ]
            },
            "between 1 and 64",
        ),
        (
            {
                "items": [
                    {"name": "same", "query": {"kind": "status"}},
                    {"name": "same", "query": {"kind": "status"}},
                ]
            },
            "duplicate",
        ),
        (
            {"items": [{"name": "", "query": {"kind": "status"}}]},
            "nonempty",
        ),
        (
            {"items": [{"name": "a" * 129, "query": {"kind": "status"}}]},
            "128",
        ),
        (
            {"items": [{"name": "bad\nname", "query": {"kind": "status"}}]},
            "control characters",
        ),
        (
            {
                "items": [
                    {
                        "name": "bad",
                        "query": {"kind": "expression", "expression": "1;Go"},
                    }
                ]
            },
            "semicolons",
        ),
        (
            {
                "items": [
                    {
                        "name": "bad",
                        "query": {"kind": "register", "register": "PC\nGo"},
                    }
                ]
            },
            "control characters",
        ),
        (
            {
                "items": [
                    {
                        "name": "bad",
                        "query": {
                            "kind": "memory",
                            "address": "0xFFFFFFFF",
                            "length": 2,
                        },
                    }
                ]
            },
            "32-bit",
        ),
        (
            {
                "items": [
                    {
                        "name": "bad",
                        "query": {
                            "kind": "memory",
                            "address": "0",
                            "length": 0,
                        },
                    }
                ]
            },
            "length",
        ),
        (
            {
                "items": [
                    {
                        "name": "bad",
                        "query": {
                            "kind": "memory",
                            "address": "0",
                            "length": 1,
                            "expected_hex": "GG",
                        },
                    }
                ]
            },
            "non-hexadecimal",
        ),
        (
            {
                "items": [
                    {
                        "name": "bad",
                        "query": {
                            "kind": "symbol",
                            "symbol": "main",
                            "expected_address": "0x100000000",
                        },
                    }
                ]
            },
            "32-bit",
        ),
        (
            {
                "items": [
                    {
                        "name": f"region{index}",
                        "query": {
                            "kind": "memory",
                            "address": "0",
                            "length": 4096,
                        },
                    }
                    for index in range(17)
                ]
            },
            "65536",
        ),
        (
            {"items": [{"name": "bad", "query": {"kind": "reset"}}]},
            "variant",
        ),
        (
            {"items": [{"name": "bad", "query": {"kind": "status", "command": "Go"}}]},
            "unknown field",
        ),
    ],
)
async def test_snapshot_invalid_requests_fail_before_connecting(
    native_mcp_server, arguments, expected
):
    async with native_mcp_server as client:
        error = await tool_error(client, "capture_snapshot", arguments)
    assert expected in error, error
    assert_validation_preceded_connection(error)


async def test_snapshot_connection_failure_keeps_independent_outcomes(
    mcp_client_factory, tmp_path, offline_port
):
    directory = tmp_path / "not-created-without-opt-in"
    request = snapshot_request()
    del request["save_artifact"]
    async with mcp_client_factory(
        "--native-tools", "--artifacts", str(directory)
    ) as client:
        result = await client.call_tool(
            "capture_snapshot", request, raise_on_error=False
        )
    assert not result.is_error
    response = result_object(result)
    assert response["format"] == "t32mcp.snapshot"
    assert response["non_atomic"] is True
    assert response["count"] == 2
    assert response["succeeded"] == 0
    assert response["failed"] == 2
    assert response["checks"]["incomplete"] == 2
    assert response["checks"]["pass"] == 0
    assert response["checks"]["fail"] == 0
    assert response["state_before"]["ok"] is False
    assert response["state_after"]["ok"] is False
    for item in response["items"]:
        assert item["read"]["ok"] is False
        assert f"127.0.0.1:{offline_port}" in item["read"]["error"]
        assert item["check"] == "incomplete"
    assert response["artifact"] == {
        "saved": False,
        "reason": "not_requested",
    }
    assert not directory.exists()


async def test_saved_snapshots_roundtrip_hash_and_diff_offline(
    mcp_client_factory, tmp_path
):
    directory = tmp_path / "snapshots"
    async with mcp_client_factory(
        "--native-tools", "--artifacts", str(directory)
    ) as client:
        before = result_object(
            await client.call_tool(
                "capture_snapshot", snapshot_request(save_artifact=True)
            )
        )
        after = result_object(
            await client.call_tool(
                "capture_snapshot", snapshot_request(save_artifact=True)
            )
        )
        before_id = assert_saved_artifact(before, directory, "snapshot")
        after_id = assert_saved_artifact(after, directory, "snapshot")
        assert before_id != after_id
        result = await client.call_tool(
            "diff_snapshots",
            {"before_id": before_id, "after_id": after_id},
            raise_on_error=False,
        )
    assert not result.is_error
    difference = result_object(result)
    assert difference["before_id"] == before_id
    assert difference["after_id"] == after_id
    assert difference["format"] == "t32mcp.snapshot_diff"
    assert difference["counts"]["unavailable"] == 2
    assert difference["counts"]["unchanged"] == 0
    assert all(item["change"] == "unavailable" for item in difference["items"])
    assert len(list(directory.glob("*.json"))) == 2


@pytest.mark.parametrize(
    "artifact_id",
    ["", "..", "../outside", r"..\outside", r"C:\outside", "a.json", "g" * 32],
)
async def test_diff_rejects_path_traversal_before_connection(
    native_mcp_server, artifact_id
):
    async with native_mcp_server as client:
        error = await tool_error(
            client,
            "diff_snapshots",
            {"before_id": artifact_id, "after_id": "0" * 32},
        )
    assert "32 hexadecimal" in error, error
    assert_validation_preceded_connection(error)


async def test_diff_rejects_missing_artifacts_offline(mcp_client_factory, tmp_path):
    async with mcp_client_factory(
        "--native-tools", "--artifacts", str(tmp_path)
    ) as client:
        error = await tool_error(
            client,
            "diff_snapshots",
            {"before_id": "0" * 32, "after_id": "1" * 32},
        )
    assert "Cannot inspect artifact" in error, error
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    "document",
    [
        {
            "format": "t32mcp.artifact",
            "version": 1,
            "id": "a" * 32,
            "kind": "image_verification",
            "payload": {},
        },
        {
            "format": "t32mcp.artifact",
            "version": 1,
            "id": "b" * 32,
            "kind": "snapshot",
            "payload": {},
        },
        {},
    ],
)
async def test_diff_rejects_wrong_kind_or_identity(
    mcp_client_factory, tmp_path, document
):
    artifact_id = "a" * 32
    (tmp_path / f"{artifact_id}.json").write_text(
        json.dumps(document), encoding="ascii"
    )
    async with mcp_client_factory(
        "--native-tools", "--artifacts", str(tmp_path)
    ) as client:
        error = await tool_error(
            client,
            "diff_snapshots",
            {"before_id": artifact_id, "after_id": artifact_id},
        )
    assert "format, ID, kind or payload" in error, error
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    ("overrides", "expected"),
    [
        ({"base_address": None}, "base_address"),
        ({"base_address": "0x100000000"}, "32-bit"),
        ({"base_address": "0xFFFFFFFF"}, "32-bit"),
        ({"allowed_ranges": []}, "between 1 and 64"),
        (
            {"allowed_ranges": [{"address": "0x1000", "length": 0}]},
            "nonempty",
        ),
        (
            {"allowed_ranges": [{"address": "0xFFFFFFFF", "length": 2}]},
            "32-bit",
        ),
        (
            {"allowed_ranges": [{"address": "0x1001", "length": 3}]},
            "not fully covered",
        ),
        (
            {"allowed_ranges": [{"address": "0x1000", "length": 3}]},
            "not fully covered",
        ),
        ({"chunk_bytes": 0}, "chunk_bytes"),
        ({"chunk_bytes": 4097}, "chunk_bytes"),
        ({"max_image_bytes": 0}, "max_image_bytes"),
        ({"max_image_bytes": 1}, "max_image_bytes"),
        ({"timeout_ms": 0}, "timeout_ms"),
        ({"timeout_ms": 60001}, "timeout_ms"),
        ({"max_mismatches": 257}, "max_mismatches"),
        ({"path": "relative.bin"}, "absolute"),
        ({"path": "//untrusted.invalid/share/image.bin"}, "network"),
        ({"path": "bad\x00path"}, "control characters"),
        ({"dual_port": True}, "unknown field"),
        ({"format": "elf"}, "variant"),
    ],
)
async def test_image_invalid_requests_fail_before_connection(
    native_mcp_server, tmp_path, overrides, expected
):
    path = tmp_path / "firmware.bin"
    path.write_bytes(b"\x01\x23\x45\x67")
    request = bin_request(path)
    request.update(overrides)
    async with native_mcp_server as client:
        error = await tool_error(client, "verify_image", request)
    assert expected in error, error
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize("file_kind", ["missing", "directory", "empty"])
async def test_image_input_must_be_existing_nonempty_regular_file(
    native_mcp_server, tmp_path, file_kind
):
    path = tmp_path / "image.bin"
    if file_kind == "directory":
        path.mkdir()
    elif file_kind == "empty":
        path.write_bytes(b"")
    async with native_mcp_server as client:
        error = await tool_error(client, "verify_image", bin_request(path))
    assert "image" in error, error
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    ("content", "expected"),
    [
        ("not hex\n", "record"),
        (":0110000001EF\n:00000001FF\n", "checksum"),
        (hex_record(0x1000, b"\x01") + "\n", "EOF"),
        (":00000001FF\n", "no image bytes"),
    ],
)
async def test_malformed_hex_is_rejected_before_connection(
    native_mcp_server, tmp_path, content, expected
):
    path = tmp_path / "firmware.hex"
    path.write_text(content, encoding="ascii")
    request = bin_request(path)
    request["format"] = "intel_hex"
    del request["base_address"]
    async with native_mcp_server as client:
        error = await tool_error(client, "verify_image", request)
    assert expected in error, error
    assert_validation_preceded_connection(error)


async def test_hex_cannot_be_silently_relocated(native_mcp_server, tmp_path):
    path = tmp_path / "firmware.hex"
    path.write_text(hex_record(0x1000, b"\x01") + "\n:00000001FF\n", encoding="ascii")
    request = bin_request(path)
    request["format"] = "intel_hex"
    async with native_mcp_server as client:
        error = await tool_error(client, "verify_image", request)
    assert "base_address must be omitted" in error
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize("save_artifact", [False, True])
async def test_valid_bin_with_absent_debugger_has_no_verified_bytes(
    mcp_client_factory, tmp_path, save_artifact, offline_port
):
    expected_bytes = b"\x01\x23\x45\x67"
    path = tmp_path / "firmware.bin"
    path.write_bytes(expected_bytes)
    directory = tmp_path / "image-evidence"
    request = bin_request(path)
    if save_artifact:
        request["save_artifact"] = True
    async with mcp_client_factory(
        "--native-tools", "--artifacts", str(directory)
    ) as client:
        result = await client.call_tool("verify_image", request, raise_on_error=False)
    assert not result.is_error
    response = result_object(result)
    assert response["kind"] == "image_verification"
    assert response["status"] == "incomplete"
    assert response["complete"] is False
    assert response["matches"] is None
    assert response["partial"] is False
    assert response["expected_bytes"] == len(expected_bytes)
    assert response["checked_bytes"] == 0
    assert response["attempted_chunks"] == 0
    assert response["checked_chunks"] == 0
    assert response["checked_ranges"] == []
    assert response["checked_payload_sha256"] is None
    assert response["expected_checked_payload_sha256"] is None
    assert (
        response["expected_payload_sha256"]
        == hashlib.sha256(expected_bytes).hexdigest()
    )
    assert response["input"]["sha256"] == hashlib.sha256(path.read_bytes()).hexdigest()
    assert response["stop_reason"]["code"] == "state_unknown"
    assert f"127.0.0.1:{offline_port}" in response["stop_reason"]["message"]
    assert response["raw_target_bytes_included"] is False
    assert response["authenticity_verified"] is False
    assert len(response["segments"]) == 1
    segment = response["segments"][0]
    assert segment["address"] == "0x00001000"
    assert segment["checked_bytes"] == 0
    assert segment["matches"] is None
    if save_artifact:
        assert_saved_artifact(response, directory, "image_verification")
    else:
        assert response["artifact"] == {
            "saved": False,
            "reason": "not_requested",
        }
        assert not directory.exists()


async def test_sparse_hex_preparation_preserves_holes_without_target_access(
    native_mcp_server, tmp_path
):
    path = tmp_path / "sparse.hex"
    content = "\n".join(
        [
            hex_record(0x1000, b"\x12"),
            hex_record(0x1004, b"\x34"),
            ":00000001FF",
            "",
        ]
    )
    path.write_bytes(content.encode("ascii"))
    request = {
        "path": str(path),
        "format": "intel_hex",
        "allowed_ranges": [
            {"address": "0x1000", "length": 1},
            {"address": "0x1004", "length": 1},
        ],
    }
    async with native_mcp_server as client:
        result = await client.call_tool("verify_image", request, raise_on_error=False)
    assert not result.is_error
    response = result_object(result)
    assert response["status"] == "incomplete"
    assert response["expected_bytes"] == 2
    assert response["checked_bytes"] == 0
    assert response["attempted_chunks"] == 0
    assert [
        (segment["address"], segment["length"]) for segment in response["segments"]
    ] == [("0x00001000", 1), ("0x00001004", 1)]
    assert (
        response["input"]["sha256"]
        == hashlib.sha256(content.encode("ascii")).hexdigest()
    )
    assert (
        response["expected_payload_sha256"] == hashlib.sha256(b"\x12\x34").hexdigest()
    )
