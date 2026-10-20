# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Offline PER protocol regressions; fixtures reserve unlistened TCP ports."""

import pytest
from test_tools import result_object, tool_error

PERIPHERAL_TOOLS = {"read_per_registers", "read_per_views"}


def registers_request(**overrides):
    return {
        "paths": ["RCC.CR", "GPIOA.MODER"],
        "acknowledge_side_effects": True,
        **overrides,
    }


def views_request(**overrides):
    return {
        "views": [{"subtree": ["RCC"]}, {"subtree": ["GPIOA"]}],
        "acknowledge_side_effects": True,
        **overrides,
    }


def assert_validation_preceded_connection(error: str):
    assert error
    assert "127.0.0.1:" not in error, error
    assert "RCL=NETTCP" not in error, error
    assert "TCP_PREFLIGHT_FAILED" not in error, error


def resolve_schema(schema: dict, root: dict) -> dict:
    reference = schema.get("$ref")
    if reference:
        assert reference.startswith("#/$defs/"), reference
        return root["$defs"][reference.removeprefix("#/$defs/")]
    return schema


@pytest.mark.parametrize(
    ("name", "required", "properties"),
    [
        (
            "read_per_registers",
            {"paths"},
            {
                "paths",
                "include_value",
                "include_choice",
                "acknowledge_side_effects",
                "timeout_ms",
            },
        ),
        (
            "read_per_views",
            {"views"},
            {"views", "acknowledge_side_effects", "timeout_ms"},
        ),
    ],
)
async def test_peripheral_tool_schemas(native_mcp_server, name, required, properties):
    async with native_mcp_server as client:
        tools = {tool.name: tool for tool in await client.list_tools()}
    schema = tools[name].inputSchema
    assert schema["type"] == "object"
    assert set(schema.get("required", [])) == required
    assert set(schema["properties"]) == properties | {"port"}
    assert schema.get("additionalProperties") is False
    assert schema["properties"]["acknowledge_side_effects"]["default"] is False
    assert schema["properties"]["timeout_ms"]["default"] == 5000
    assert not {"command", "script", "expression", "file"} & set(schema["properties"])


async def test_per_view_selection_has_closed_schema(native_mcp_server):
    async with native_mcp_server as client:
        tools = {tool.name: tool for tool in await client.list_tools()}
    schema = tools["read_per_views"].inputSchema
    selection = resolve_schema(schema["properties"]["views"]["items"], schema)
    assert selection["type"] == "object"
    assert set(selection["properties"]) == {"subtree", "expand", "offset", "max_bytes"}
    assert not selection.get("required")
    assert selection.get("additionalProperties") is False
    assert selection["properties"]["expand"]["default"] is False
    assert selection["properties"]["offset"]["default"] == 0
    assert selection["properties"]["max_bytes"]["default"] == 4096


async def test_peripheral_tools_remain_available_read_only(mcp_client_factory):
    async with mcp_client_factory("--native-tools", "--read-only") as client:
        names = {tool.name for tool in await client.list_tools()}
        register_error = await tool_error(
            client, "read_per_registers", {"paths": ["RCC.CR"]}
        )
        view_error = await tool_error(
            client, "read_per_views", {"views": [{"subtree": ["RCC"]}]}
        )
    assert PERIPHERAL_TOOLS <= names
    for error in (register_error, view_error):
        assert "acknowledge_side_effects" in error
        assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    ("name", "arguments"),
    [
        ("read_per_registers", {"paths": ["RCC.CR"]}),
        (
            "read_per_registers",
            registers_request(acknowledge_side_effects=False),
        ),
        ("read_per_views", {"views": [{"subtree": ["RCC"]}]}),
        ("read_per_views", views_request(acknowledge_side_effects=False)),
    ],
)
async def test_peripheral_reads_require_explicit_side_effect_acknowledgment(
    native_mcp_server, name, arguments
):
    async with native_mcp_server as client:
        error = await tool_error(client, name, arguments)
    assert "acknowledge_side_effects" in error, error
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    "overrides",
    [
        {"paths": []},
        {"paths": ["RCC.CR"] * 65},
        {"paths": [""]},
        {"paths": ["   "]},
        {"paths": ["R" * 513]},
        {"paths": ["RCC.CR", "RCC.CR"]},
        {"paths": [" RCC.CR"]},
        {"paths": ["RCC.CR "]},
        {"paths": ["..."]},
        {"timeout_ms": 0},
        {"timeout_ms": 60001},
    ],
)
async def test_per_register_bounds_rejected_before_connecting(
    native_mcp_server, overrides
):
    async with native_mcp_server as client:
        error = await tool_error(
            client, "read_per_registers", registers_request(**overrides)
        )
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    "path",
    [
        "RCC.CR;Go",
        'RCC.CR");Go',
        "RCC.'CR'",
        "RCC.CR\nGo",
        "RCC.CR\rGo",
        "RCC.CR\x00",
        "RCC.CR\t",
        "RCC.&register",
        "RCC.%register",
        "RCC.$register",
        "RCC.`register",
        "RCC.CR,GPIOA.MODER",
        "RCC.*",
        "RCC.CR?",
        "RCC/CR",
        "RCC\\CR",
        "RCC.\u03a9",
    ],
)
async def test_per_register_injection_in_late_item_rejects_entire_batch(
    native_mcp_server, path
):
    async with native_mcp_server as client:
        error = await tool_error(
            client,
            "read_per_registers",
            registers_request(paths=["RCC.CR", "GPIOA.MODER", path]),
        )
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    "overrides",
    [
        {"views": []},
        {"views": [{"subtree": ["RCC"]}] * 17},
        {"views": [{"subtree": ["RCC"] * 17}]},
        {"views": [{"subtree": [""]}]},
        {"views": [{"subtree": ["   "]}]},
        {"views": [{"subtree": ["R" * 129]}]},
        {"views": [{"subtree": [], "expand": True}]},
        {"views": [{"expand": True}]},
        {"views": [{"subtree": ["RCC"], "max_bytes": 0}]},
        {"views": [{"subtree": ["RCC"], "max_bytes": 8193}]},
        {"views": [{"subtree": ["RCC"], "offset": 4294967295, "max_bytes": 1}]},
        {"views": [{"subtree": ["RCC"], "max_bytes": 8192}] * 9},
        {"timeout_ms": 0},
        {"timeout_ms": 60001},
    ],
)
async def test_per_view_bounds_rejected_before_connecting(native_mcp_server, overrides):
    async with native_mcp_server as client:
        error = await tool_error(client, "read_per_views", views_request(**overrides))
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    "component",
    [
        "RCC;Go",
        'RCC", "GPIOA',
        "'RCC'",
        "RCC\nGo",
        "RCC\rGo",
        "RCC\x00",
        "RCC\t",
        "&peripheral",
        "RCC,GPIOA",
        "*",
        "RCC?",
        "RCC/Reset",
        "RCC\\Reset",
        "RCC\u03a9",
    ],
)
async def test_per_view_injection_in_late_selection_rejects_entire_batch(
    native_mcp_server, component
):
    async with native_mcp_server as client:
        error = await tool_error(
            client,
            "read_per_views",
            views_request(
                views=[
                    {"subtree": ["RCC"]},
                    {"subtree": ["GPIOA", component]},
                ]
            ),
        )
    assert_validation_preceded_connection(error)


@pytest.mark.parametrize(
    ("name", "arguments", "expected"),
    [
        ("read_per_registers", {}, "paths"),
        ("read_per_registers", registers_request(paths="RCC.CR"), "sequence"),
        ("read_per_registers", registers_request(paths=[1]), "string"),
        (
            "read_per_registers",
            registers_request(include_value="false"),
            "boolean",
        ),
        (
            "read_per_registers",
            registers_request(include_choice=1),
            "boolean",
        ),
        (
            "read_per_registers",
            registers_request(acknowledge_side_effects="true"),
            "boolean",
        ),
        (
            "read_per_registers",
            registers_request(command="Go"),
            "unknown field",
        ),
        (
            "read_per_registers",
            registers_request(timeout_ms=-1),
            "u64",
        ),
        ("read_per_views", {}, "views"),
        ("read_per_views", views_request(views="RCC"), "sequence"),
        (
            "read_per_views",
            views_request(views=[{"subtree": "RCC"}]),
            "sequence",
        ),
        (
            "read_per_views",
            views_request(views=[{"subtree": [1]}]),
            "string",
        ),
        (
            "read_per_views",
            views_request(views=[{"subtree": ["RCC"], "expand": "true"}]),
            "boolean",
        ),
        (
            "read_per_views",
            views_request(views=[{"subtree": ["RCC"], "command": "Go"}]),
            "unknown field",
        ),
        (
            "read_per_views",
            views_request(command="Go"),
            "unknown field",
        ),
        (
            "read_per_views",
            views_request(acknowledge_side_effects="true"),
            "boolean",
        ),
    ],
)
async def test_peripheral_schema_errors_are_protocol_errors(
    native_mcp_server, name, arguments, expected
):
    async with native_mcp_server as client:
        error = await tool_error(client, name, arguments)
    assert expected in error, error
    assert_validation_preceded_connection(error)


def assert_register_connection_failure(response: dict, arguments: dict):
    assert response["format"] == "t32mcp.peripheral-registers"
    assert response["version"] == 1
    assert response["status"] == "connection_failed"
    assert response["ok"] is False
    assert response["non_atomic"] is True
    assert response["definition_source"] == "default_per"
    assert response["acknowledged_side_effects"] is True
    assert response["deadline_kind"] == "soft_between_native_calls"
    assert response["state_before"] is None
    assert response["last_state_observation"] is None
    assert response["counts"] == {
        "requested": len(arguments["paths"]),
        "succeeded": 0,
        "partial": 0,
        "failed": 0,
        "not_attempted": len(arguments["paths"]),
        "evaluations": 0,
        "state_probes": 0,
    }
    assert response["stop_reason"]["code"] == "CONNECTION_FAILED"
    assert response["stop_reason"]["message"]
    assert [item["path"] for item in response["results"]] == arguments["paths"]
    for item in response["results"]:
        assert item["status"] == "not_attempted"
        assert item["ok"] is False
        assert item["state_stable"] is False
        assert item["started_unix_ms"] is None
        assert item["completed_unix_ms"] is None
        for field, requested in (
            ("address", True),
            ("state_before", True),
            ("state_after", True),
            ("value", arguments.get("include_value", True)),
            ("choice", arguments.get("include_choice", False)),
        ):
            if requested:
                assert item[field] == {
                    "status": "not_attempted",
                    "ok": False,
                    "reason_code": "CONNECTION_FAILED",
                }
            else:
                assert item[field] == {"status": "not_requested"}


@pytest.mark.parametrize(
    "arguments",
    [
        pytest.param(registers_request(), id="default-value"),
        pytest.param(
            registers_request(include_value=False, include_choice=False),
            id="address-only",
        ),
        pytest.param(
            registers_request(include_value=True, include_choice=True),
            id="value-and-choice",
        ),
        pytest.param(
            registers_request(include_value=False, include_choice=True),
            id="choice-only",
        ),
        pytest.param(
            registers_request(
                paths=["R" * 509 + f"{index:03}" for index in range(64)],
                timeout_ms=60000,
            ),
            id="maximum-path-count-and-length",
        ),
        pytest.param(
            registers_request(paths=[".DATA[0]:Low-byte"], timeout_ms=1),
            id="minimum-timeout-and-supported-path-punctuation",
        ),
        pytest.param(
            registers_request(paths=["Memory Protection Unit (MPU).MPU_TYPE"]),
            id="parenthesized-vendor-title",
        ),
    ],
)
async def test_per_register_connection_failure_preserves_each_requested_field(
    native_mcp_server, arguments
):
    async with native_mcp_server as client:
        result = await client.call_tool(
            "read_per_registers", arguments, raise_on_error=False
        )
    assert not result.is_error
    assert_register_connection_failure(result_object(result), arguments)


def assert_view_connection_failure(response: dict, arguments: dict):
    assert response["status"] == "stopped"
    assert response["ok"] is False
    assert response["completed"] is False
    assert response["atomic"] is False
    assert response["soft_timeout"] is True
    assert response["target_state"] == {"initial": None, "last": None, "checks": 0}
    assert response["stop_reason"]["code"] == "connection_failed"
    assert response["stop_reason"]["phase"] == "connect"
    assert response["stop_reason"]["view_index"] == 0
    assert response["stop_reason"]["message"]
    assert response["summary"] == {
        "requested": len(arguments["views"]),
        "attempted": 0,
        "succeeded": 0,
        "failed": 0,
        "not_attempted": len(arguments["views"]),
        "total_requested_bytes": sum(
            view.get("max_bytes", 4096) for view in arguments["views"]
        ),
    }
    assert len(response["results"]) == len(arguments["views"])
    for index, (item, view) in enumerate(zip(response["results"], arguments["views"])):
        subtree = view.get("subtree", [])
        normalized = {
            "subtree": subtree,
            "expand": view.get("expand", False),
            "offset": view.get("offset", 0),
            "max_bytes": view.get("max_bytes", 4096),
        }
        assert item["index"] == index
        assert item["query"] == normalized
        command = "PER.view"
        if subtree:
            path = ",".join(subtree)
            if normalized["expand"]:
                path += ",*"
            command += f' , "{path}"'
        assert item["command"] == command
        assert item["status"] == "not_attempted"
        assert item["ok"] is False
        assert item["attempted"] is False
        assert item["state_verified"] is False
        for field in ("text", "bytes_read", "next_offset", "may_have_more"):
            assert item[field] is None
        assert item["error"]["code"] == "not_attempted"
        assert item["error"]["stop_code"] == "connection_failed"


@pytest.mark.parametrize(
    "arguments",
    [
        pytest.param(
            views_request(
                views=[
                    {},
                    {"subtree": ["RCC"], "expand": True},
                    {"subtree": ["GPIOA", "Port [0]:Low-byte"]},
                ]
            ),
            id="root-subtree-and-controlled-expansion",
        ),
        pytest.param(
            views_request(
                views=[{"subtree": ["R" * 128] * 16}] * 16,
                timeout_ms=60000,
            ),
            id="maximum-views-components-and-total-bytes",
        ),
        pytest.param(
            views_request(views=[{"subtree": ["RCC"], "max_bytes": 8192}] * 8),
            id="maximum-page-and-total-bytes",
        ),
        pytest.param(
            views_request(
                views=[
                    {
                        "subtree": ["RCC"],
                        "offset": 4294967294,
                        "max_bytes": 1,
                    }
                ],
                timeout_ms=1,
            ),
            id="maximum-page-end-and-minimum-timeout",
        ),
        pytest.param(
            views_request(views=[{"subtree": ["Memory Protection Unit (MPU)"]}]),
            id="parenthesized-vendor-title",
        ),
    ],
)
async def test_per_view_connection_failure_preserves_each_selection(
    native_mcp_server, arguments
):
    async with native_mcp_server as client:
        result = await client.call_tool(
            "read_per_views", arguments, raise_on_error=False
        )
    assert not result.is_error
    assert_view_connection_failure(result_object(result), arguments)


async def test_acknowledged_peripheral_requests_work_in_read_only_mode(
    mcp_client_factory,
):
    register_arguments = registers_request()
    view_arguments = views_request()
    async with mcp_client_factory("--native-tools", "--read-only") as client:
        registers = await client.call_tool(
            "read_per_registers", register_arguments, raise_on_error=False
        )
        views = await client.call_tool(
            "read_per_views", view_arguments, raise_on_error=False
        )
    assert not registers.is_error
    assert not views.is_error
    assert_register_connection_failure(result_object(registers), register_arguments)
    assert_view_connection_failure(result_object(views), view_arguments)
