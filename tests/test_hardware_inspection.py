# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Offline hardware-probe regressions: fake clients never open a debugger port."""

import json
from datetime import UTC, datetime
from types import SimpleNamespace

import hardware_inspection as probe
import pytest

TOOLS = {
    "get_debugger_status",
    "evaluate_expressions",
    "wait_for_condition",
    "lookup_symbols",
    "list_practice_skills",
    "read_memory",
    "read_debugger_window",
}


def batch(results):
    return {
        "count": len(results),
        "succeeded": len(results),
        "failed": 0,
        "results": [dict(ok=True, **result) for result in results],
    }


def arguments(tmp_path, *extra):
    return probe.parse_args(
        [
            "--phase",
            "queries",
            "--port",
            "1",
            "--output",
            str(tmp_path / "probe.json"),
            *extra,
        ]
    )


class FakeClient:
    def __init__(
        self,
        output,
        *,
        state=3,
        final_state=None,
        responses=None,
        tools=None,
        connect_error=None,
    ):
        self.output = output
        self.initial_state = state
        self.final_state = state if final_state is None else final_state
        self.responses = responses or {}
        self.tools = TOOLS if tools is None else tools
        self.connect_error = connect_error
        self.calls = []
        self.snapshots = []
        self.status_calls = 0
        self.closed = False

    def pending_checkpoint(self, label=None):
        saved = json.loads(self.output.read_text(encoding="utf-8"))
        operation = saved["operations"][-1]
        assert operation["status"] == "pending"
        if label is not None:
            assert operation["label"] == label
        assert "started_utc" in operation
        self.snapshots.append(saved)
        return operation

    async def __aenter__(self):
        self.pending_checkpoint("connect")
        if self.connect_error:
            raise self.connect_error
        return self

    async def __aexit__(self, *_):
        self.pending_checkpoint("disconnect")
        self.closed = True

    async def list_tools(self):
        self.pending_checkpoint("list_tools")
        return [SimpleNamespace(name=name) for name in sorted(self.tools)]

    async def call_tool(self, name, args, raise_on_error):
        entry = self.pending_checkpoint()
        assert raise_on_error is False
        assert entry["tool"] == name
        assert entry["arguments"] == args
        assert name not in probe.FORBIDDEN_TOOLS
        self.calls.append((name, args))
        override = self.responses.get(entry["label"])
        if isinstance(override, Exception):
            raise override
        if callable(override):
            result = override(args)
        elif override is not None:
            result = override
        elif name == "get_debugger_status":
            state = self.initial_state if self.status_calls == 0 else self.final_state
            self.status_calls += 1
            result = {"state": {"ok": True, "value": state}}
        elif name == "evaluate_expressions":
            result = batch([{"value": value} for value in ("TRUE()", "0.", "2.")])
        elif name == "wait_for_condition":
            match = args["expected_value"] != "not-a-number"
            result = {
                "matched": match,
                "timed_out": not match,
                "attempts": 1 if match else 3,
                "expected_value": args["expected_value"],
            }
        elif name == "lookup_symbols":
            result = batch([{"address": 4096} for _ in args["names"]])
        elif name == "list_practice_skills":
            result = {"skills": [{"skill_name": "fixture", "scripts": ["read.cmm"]}]}
        elif name == "read_memory":
            region = args["regions"][0]
            actual = "ab" * region["length"]
            result = batch([{"hex": actual}])
            if "expected_hex" in region:
                match = actual == region["expected_hex"]
                result["results"][0].update(
                    matches=match,
                    mismatch_count=0 if match else 1,
                    mismatch_offsets=[] if match else [0],
                )
        elif name == "read_debugger_window":
            count = 3 if args["offset"] == 0 else 0
            result = {
                "window": args["window"],
                "offset": args["offset"],
                "bytes_read": count,
                "text": "abc" if count else "",
                "may_have_more": count > 0,
                "next_offset": args["offset"] + count if count else None,
            }
        else:
            raise AssertionError(name)
        if isinstance(result, SimpleNamespace):
            return result
        return SimpleNamespace(is_error=False, structured_content=result, content=[])


async def fake_run(args, **kwargs):
    client = FakeClient(args.output, **kwargs)
    transport_seen = []

    def factory(transport):
        transport_seen.append(transport)
        return client

    report = await probe.run(args, client_factory=factory)
    assert json.loads(args.output.read_text(encoding="utf-8")) == report
    assert "--read-only" in transport_seen[0].args
    assert "--native-tools" in transport_seen[0].args
    assert transport_seen[0].args[transport_seen[0].args.index("--port") + 1] == "1"
    return report, client, transport_seen[0]


@pytest.mark.parametrize("missing", ["--port", "--output", "--phase"])
def test_required_arguments(tmp_path, missing):
    argv = ["--phase", "queries", "--port", "1", "--output", str(tmp_path / "x.json")]
    index = argv.index(missing)
    del argv[index : index + 2]
    with pytest.raises(SystemExit):
        probe.parse_args(argv)


@pytest.mark.parametrize(
    "extra",
    [
        ["--phase", "memory"],
        ["--phase", "memory", "--address", "0"],
        ["--phase", "memory", "--length", "1"],
        ["--phase", "memory", "--address", "-1", "--length", "1"],
        ["--phase", "memory", "--address", "0x100000000", "--length", "1"],
        ["--phase", "memory", "--address", "0xffffffff", "--length", "2"],
        ["--phase", "memory", "--address", "0", "--length", "4097"],
        ["--phase", "memory", "--address", "0", "--length", "0"],
        ["--max-pages", "9"],
        ["--page-bytes", "8193"],
        ["--port", "65536"],
        ["--phase", "queries", "--dual-port"],
        ["--phase", "queries", "--address", "0"],
        ["--phase", "windows", "--symbol", "main"],
    ],
)
def test_parser_rejects_unsafe_or_unbounded_requests(tmp_path, extra):
    with pytest.raises(SystemExit):
        arguments(tmp_path, *extra)


def test_memory_accepts_last_32_bit_byte_and_explicit_max_length(tmp_path):
    args = arguments(
        tmp_path, "--phase", "memory", "--address", "0xffffffff", "--length", "1"
    )
    assert args.address == 0xFFFFFFFF
    args = arguments(
        tmp_path, "--phase", "memory", "--address", "0", "--length", "4096"
    )
    assert args.length == 4096


async def test_queries_checkpoint_every_call_and_use_real_utc(tmp_path):
    args = arguments(tmp_path, "--symbol", "main", "--skills", str(tmp_path / "skills"))
    started = datetime.now(UTC)
    report, client, transport = await fake_run(args)
    completed = datetime.now(UTC)
    assert report["status"] == "pass"
    assert report["passed"] is True
    assert report["state_unchanged"] is True
    assert "--skills" in transport.args
    assert client.closed
    assert len(client.snapshots) == len(report["operations"])
    for operation in report["operations"]:
        assert operation["status"] == "ok"
        assert started <= datetime.fromisoformat(operation["started_utc"]) <= completed
        assert (
            started <= datetime.fromisoformat(operation["completed_utc"]) <= completed
        )
    assert not list(tmp_path.glob("*.tmp"))
    assert not any(
        name in {"lookup_symbols", "list_practice_skills"}
        for name, _ in (await fake_run(arguments(tmp_path)))[1].calls
    )


async def test_connection_failure_is_checkpointed_and_incomplete(tmp_path):
    report, client, _ = await fake_run(
        arguments(tmp_path),
        connect_error=ConnectionError("server unavailable"),
    )
    assert report["status"] == "incomplete"
    assert report["passed"] is False
    assert report["state_unchanged"] is None
    assert report["operations"][0]["status"] == "incomplete"
    assert "server unavailable" in report["operations"][0]["error"]
    assert not client.calls


async def test_missing_final_state_is_unknown_not_changed(tmp_path):
    report, _, _ = await fake_run(
        arguments(tmp_path),
        responses={"after": ConnectionError("lost debugger")},
    )
    assert report["state_unchanged"] is None
    assert report["status"] == "incomplete"
    assert report["passed"] is False
    after = next(item for item in report["operations"] if item["label"] == "after")
    assert after["status"] == "incomplete"
    assert "completed_utc" in after


async def test_status_tool_connection_error_is_incomplete_not_a_capability_failure(
    tmp_path,
):
    error = SimpleNamespace(
        is_error=True,
        content=[SimpleNamespace(type="text", text="Cannot reach TRACE32")],
    )
    report, _, _ = await fake_run(
        arguments(tmp_path), responses={"before": error, "after": error}
    )
    assert report["status"] == "incomplete"
    assert report["state_unchanged"] is None
    assert report["phase_complete"] is False


async def test_client_timeout_is_checkpointed_without_claiming_native_completion(
    tmp_path,
):
    report, _, _ = await fake_run(
        arguments(tmp_path),
        responses={"batch_queries": TimeoutError("client deadline")},
    )
    assert report["status"] == "incomplete"
    operation = next(
        item for item in report["operations"] if item["label"] == "batch_queries"
    )
    assert operation["status"] == "incomplete"
    assert "TimeoutError" in operation["error"]
    assert "completed_utc" in operation


async def test_observed_state_change_fails(tmp_path):
    report, _, _ = await fake_run(arguments(tmp_path), final_state=2)
    assert report["state_unchanged"] is False
    assert report["status"] == "fail"
    assert report["passed"] is False


async def test_execution_tool_exposure_stops_before_debugger_queries(tmp_path):
    report, client, _ = await fake_run(
        arguments(tmp_path),
        tools=TOOLS | {"execute_practice"},
    )
    assert report["status"] == "fail"
    assert report["checks"]["read_only_tools"]["status"] == "fail"
    assert not client.calls


async def test_failed_initial_state_never_vacuously_passes(tmp_path):
    report, client, _ = await fake_run(
        arguments(tmp_path),
        responses={"before": {"state": {"ok": False, "error": "offline"}}},
    )
    assert report["status"] == "incomplete"
    assert report["state_unchanged"] is None
    assert [name for name, _ in client.calls] == [
        "get_debugger_status",
        "get_debugger_status",
    ]
    assert report["phase_complete"] is False


@pytest.mark.parametrize(
    "extra,state",
    [
        ([], 3),
        (["--dual-port"], 3),
        (["--access", "arm_dap"], 3),
        (["--access", "program"], 3),
        (["--access", "program", "--dual-port"], 3),
        ([], 0),
        (["--access", "arm_dap", "--dual-port"], 0),
        (["--access", "arm_dap", "--dual-port"], 4),
    ],
)
async def test_running_or_unavailable_target_blocks_unsafe_memory_access(
    tmp_path, extra, state
):
    args = arguments(
        tmp_path,
        "--phase",
        "memory",
        "--address",
        "4096",
        "--length",
        "4",
        *extra,
    )
    report, client, _ = await fake_run(args, state=state)
    assert report["status"] == "unsupported"
    assert report["passed"] is False
    assert report["state_unchanged"] is True
    assert not any(name == "read_memory" for name, _ in client.calls)


@pytest.mark.parametrize(
    "extra,state",
    [
        (["--access", access] + (["--dual-port"] if dual else []), state)
        for state in (1, 2)
        for access in ("data", "program", "arm_dap")
        for dual in (False, True)
    ]
    + [(["--access", "arm_dap", "--dual-port"], 3)],
)
async def test_explicit_memory_and_opt_in_comparisons(tmp_path, extra, state):
    args = arguments(
        tmp_path,
        "--phase",
        "memory",
        "--address",
        "0x1000",
        "--length",
        "4",
        "--compare",
        *extra,
    )
    report, client, _ = await fake_run(args, state=state)
    assert report["status"] == "pass"
    reads = [args["regions"][0] for name, args in client.calls if name == "read_memory"]
    assert len(reads) == 3
    assert all(item["address"] == "0x1000" and item["length"] == 4 for item in reads)
    assert reads[1]["expected_hex"] == "abababab"
    assert reads[2]["expected_hex"] == "54ababab"


async def test_memory_tool_failure_is_distinct_from_transport_loss(tmp_path):
    args = arguments(
        tmp_path,
        "--phase",
        "memory",
        "--address",
        "4096",
        "--length",
        "4",
        "--access",
        "arm_dap",
        "--dual-port",
    )
    error = SimpleNamespace(
        is_error=True,
        content=[SimpleNamespace(type="text", text="Remote API error -1")],
    )
    report, _, _ = await fake_run(
        args,
        responses={"memory_read": error, "after": ConnectionError("lost debugger")},
    )
    assert report["status"] == "fail"
    assert report["checks"]["memory_read"]["status"] == "fail"
    assert report["state_unchanged"] is None


async def test_missing_requested_tool_is_unsupported(tmp_path):
    args = arguments(tmp_path, "--phase", "windows")
    report, client, _ = await fake_run(args, tools=TOOLS - {"read_debugger_window"})
    assert report["status"] == "unsupported"
    assert not any(name == "read_debugger_window" for name, _ in client.calls)


async def test_default_windows_only_break_list_and_follow_short_page(tmp_path):
    report, client, _ = await fake_run(arguments(tmp_path, "--phase", "windows"))
    assert report["status"] == "pass"
    pages = [args for name, args in client.calls if name == "read_debugger_window"]
    assert pages == [
        {"window": "break_list", "offset": 0, "max_bytes": 128},
        {"window": "break_list", "offset": 3, "max_bytes": 128},
    ]
    assert report["checks"]["window_break_list"]["detail"]["more_available"] is False


async def test_windows_are_opt_in_and_page_count_is_bounded(tmp_path):
    args = arguments(
        tmp_path,
        "--phase",
        "windows",
        "--window",
        "frame",
        "--window",
        "frame",
        "--max-pages",
        "1",
    )
    report, client, _ = await fake_run(args)
    assert report["status"] == "pass"
    pages = [args for name, args in client.calls if name == "read_debugger_window"]
    assert len(pages) == 1
    assert pages[0]["window"] == "frame"
    assert report["checks"]["window_frame"]["detail"]["more_available"] is True


async def test_native_byte_offsets_need_not_equal_decoded_text_length(tmp_path):
    args = arguments(tmp_path, "--phase", "windows", "--max-pages", "1")
    page = {
        "window": "break_list",
        "offset": 0,
        "bytes_read": 1,
        "text": "\ufffd",
        "may_have_more": True,
        "next_offset": 1,
    }
    report, _, _ = await fake_run(args, responses={"window_break_list_page_1": page})
    assert report["status"] == "pass"


async def test_invalid_window_pagination_fails(tmp_path):
    args = arguments(tmp_path, "--phase", "windows")
    page = {
        "window": "break_list",
        "offset": 0,
        "bytes_read": 3,
        "text": "abc",
        "may_have_more": False,
        "next_offset": None,
    }
    report, client, _ = await fake_run(
        args, responses={"window_break_list_page_1": page}
    )
    assert report["status"] == "fail"
    assert (
        len([name for name, _ in client.calls if name == "read_debugger_window"]) == 1
    )
