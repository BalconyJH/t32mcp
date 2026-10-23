# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Request deadlines are exercised only against test-owned TCP listeners."""

import asyncio
import socket
import subprocess
import threading
import time

import pytest
from fastmcp import Client
from fastmcp.client.transports import StdioTransport
from test_tools import tool_error


@pytest.fixture
def blackhole():
    """Accept and hold connections without answering the RCL handshake."""
    stopped = threading.Event()
    accepted = []
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(0.05)

        def hold():
            while not stopped.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                accepted.append(connection)

        thread = threading.Thread(target=hold, daemon=True)
        thread.start()
        try:
            yield listener.getsockname()[1], accepted
        finally:
            stopped.set()
            thread.join(timeout=2)
            assert not thread.is_alive()
            for connection in accepted:
                connection.close()


@pytest.fixture
def deadline_client_factory(server_executable, skills_base, tmp_path, blackhole):
    port, _ = blackhole

    def factory(*extra_args):
        return Client(
            StdioTransport(
                command=server_executable,
                args=[
                    "--execution-policy", "unrestricted",
                    "--port", str(port),
                    "--skills", str(skills_base),
                    "--artifacts", str(tmp_path / "deadline-artifacts"),
                    *extra_args,
                ],
            )
        )

    return factory


@pytest.fixture(
    params=["script", "collected-command", "fire-and-forget-command", "status"]
)
def deadline_request(request, skills_base):
    if request.param == "script":
        return (
            "trace32_run_script",
            {"script_path": str(skills_base / "fixture" / "scripts" / "read.cmm")},
        )
    if request.param == "status":
        return "trace32_get_status", {}
    return (
        "trace32_run_command",
        {
            "command": "PRINT 1.",
            "collect_output": request.param == "collected-command",
        },
    )


async def assert_worker_is_available(client, skills_base, offline_port):
    # This error needs the worker but never a TCP connection. It detects a
    # caller-only timeout that leaves the worker in the old five-second attach.
    error = await tool_error(
        client, "trace32_run_script",
        {
            "script_path": str(skills_base / "fixture" / "scripts" / "read.cmm"),
            "port": offline_port,
            "timeout": 1,
        },
    )
    assert f"No instance with port {offline_port} is added" in error, error


async def test_tool_timeout_covers_handshake_and_releases_the_worker(
    deadline_client_factory, blackhole, deadline_request, skills_base, offline_port
):
    tool_name, arguments = deadline_request
    async with deadline_client_factory("--request-timeout", "0.05") as client:
        async with asyncio.timeout(8):
            started = time.monotonic()
            error = await tool_error(client, tool_name, {**arguments, "timeout": 1})
            elapsed = time.monotonic() - started
            assert "unknown_completion" in error, error
            assert "Do not automatically retry" in error, error
            assert 0.5 <= elapsed < 3, elapsed
            await assert_worker_is_available(client, skills_base, offline_port)
    assert blackhole[1], "the test must actually exercise connection setup"


async def test_management_uses_the_configured_request_budget(
    deadline_client_factory, blackhole, skills_base, offline_port
):
    async with deadline_client_factory("--request-timeout", "0.2") as client:
        async with asyncio.timeout(8):
            started = time.monotonic()
            error = await tool_error(
                client, "trace32_manage_instances", {"action": "inspect"}
            )
            elapsed = time.monotonic() - started
            assert "unknown_completion" in error, error
            assert elapsed < 3, elapsed
            await assert_worker_is_available(client, skills_base, offline_port)
    assert blackhole[1]


async def test_zero_tool_budget_never_starts_a_connection(
    deadline_client_factory, blackhole, deadline_request
):
    tool_name, arguments = deadline_request
    async with deadline_client_factory() as client:
        async with asyncio.timeout(5):
            error = await tool_error(client, tool_name, {**arguments, "timeout": 0})
    assert "not_started" in error, error
    assert not blackhole[1]


@pytest.mark.parametrize(
    "value", ["0", "-1", "NaN", "inf", "-inf", "1e999", "1e30", "garbage", ""]
)
def test_invalid_request_timeout_is_a_clean_cli_error(
    server_executable, offline_port, value
):
    process = subprocess.run(
        [server_executable, "--port", str(offline_port), "--request-timeout", value],
        stdin=subprocess.DEVNULL, capture_output=True, text=True,
        timeout=5, check=False,
    )
    assert process.returncode != 0
    assert "request-timeout" in process.stderr, process.stderr
    assert "panicked at" not in process.stderr


def test_request_timeout_requires_a_value(server_executable):
    process = subprocess.run(
        [server_executable, "--request-timeout"],
        stdin=subprocess.DEVNULL, capture_output=True, text=True,
        timeout=5, check=False,
    )
    assert process.returncode != 0
    assert "request-timeout" in process.stderr, process.stderr
    assert "panicked at" not in process.stderr
