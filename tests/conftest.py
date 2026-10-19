# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Offline MCP fixtures: never use a developer's active TRACE32 API port."""

import logging
import os
import socket
import subprocess
import tempfile
import time
from contextlib import contextmanager
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import urlopen

import pytest
from fastmcp import Client
from fastmcp.client.transports import StdioTransport, StreamableHttpTransport

LOGGER = logging.getLogger("mcp")
ROOT = Path(__file__).resolve().parents[1]

NATIVE_TOOLS = {
    "read_per_registers",
    "read_per_views",
    "doctor",
    "capture_snapshot",
    "diff_snapshots",
    "verify_image",
    "get_debugger_status",
    "evaluate_expressions",
    "read_registers",
    "read_memory",
    "lookup_symbols",
    "read_debugger_window",
    "wait_for_condition",
    "list_practice_skills",
}
DEFAULT_TOOLS = {
    "execute_practice_skill",
    "collect_practice_skill_response",
    "abort_practice_skill",
}
READ_ONLY_TOOLS = {"collect_practice_skill_response"}


def t32mcp_path(request=None) -> str:
    configured = os.environ.get("T32MCP")
    if configured:
        path = Path(configured.strip().strip('"').strip("'")).expanduser()
    else:
        filename = "t32mcp.exe" if os.name == "nt" else "t32mcp"
        path = ROOT / "t32mcp-rs" / "target" / "debug" / filename
    path = path.resolve()
    if not path.is_file():
        pytest.fail(f"Build t32mcp first or set T32MCP to its executable: {path}")
    return str(path)


@pytest.fixture(scope="session")
def server_executable() -> str:
    return t32mcp_path()


@pytest.fixture(scope="session")
def skills_base(tmp_path_factory) -> Path:
    base = tmp_path_factory.mktemp("skills with spaces")
    scripts = base / "skill-fixture" / "scripts"
    scripts.mkdir(parents=True)
    (scripts / "read.cmm").write_text("", encoding="ascii")
    (scripts / "ignored.txt").write_text("", encoding="ascii")
    return base


@pytest.fixture(scope="session")
def offline_server_args(skills_base, tmp_path_factory) -> list[str]:
    # Explicit isolation protects a connected board even if validation regresses.
    evidence = tmp_path_factory.mktemp("offline evidence")
    return [
        "--port",
        "1",
        "--skills",
        str(skills_base),
        "--artifacts",
        str(evidence),
    ]


@pytest.fixture
def mcp_client_factory(server_executable, offline_server_args):
    def factory(*extra_args: str) -> Client:
        return Client(
            StdioTransport(
                command=server_executable,
                args=[*offline_server_args, *extra_args],
            )
        )

    return factory


@pytest.fixture(scope="module")
def local_mcp_server(server_executable, offline_server_args) -> Client:
    LOGGER.info("Setting up offline STDIO server: %s", server_executable)
    return Client(
        StdioTransport(command=server_executable, args=list(offline_server_args))
    )


@pytest.fixture(scope="module")
def native_mcp_server(server_executable, offline_server_args) -> Client:
    return Client(
        StdioTransport(
            command=server_executable,
            args=[*offline_server_args, "--native-tools"],
        )
    )


def _available_http_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def _wait_for_http_server(process, url: str, log_file, timeout: float = 15) -> None:
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            break
        try:
            with urlopen(url, timeout=0.25) as response:
                if response.status == 200:
                    return
        except HTTPError as error:
            # An MCP-specific rejection proves the endpoint is serving requests.
            if error.code in {400, 405, 406, 415}:
                return
            last_error = error
        except (URLError, TimeoutError, OSError) as error:
            last_error = error
        time.sleep(min(0.05, max(0, deadline - time.monotonic())))
    log_file.flush()
    log_file.seek(0)
    diagnostics = log_file.read().decode("utf-8", errors="replace")
    pytest.fail(
        f"HTTP MCP server did not become ready at {url}; "
        f"exit={process.poll()}, last_error={last_error}\n{diagnostics}"
    )


def _stop_owned_process(process: subprocess.Popen) -> None:
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


@pytest.fixture(scope="module")
def http_mcp_client_factory(server_executable, offline_server_args):
    @contextmanager
    def factory(*extra_args: str):
        port = _available_http_port()
        url = f"http://127.0.0.1:{port}/mcp"
        argv = [
            server_executable,
            *offline_server_args,
            *extra_args,
            "--http",
            str(port),
        ]
        LOGGER.info("Setting up offline HTTP server: %s", argv)
        with tempfile.TemporaryFile() as log_file:
            process = subprocess.Popen(
                argv,
                stdin=subprocess.DEVNULL,
                stdout=log_file,
                stderr=log_file,
            )
            try:
                _wait_for_http_server(process, url, log_file)
                yield Client(StreamableHttpTransport(url=url))
            finally:
                _stop_owned_process(process)

    return factory


@pytest.fixture(scope="module")
def remote_mcp_server(http_mcp_client_factory) -> Client:
    with http_mcp_client_factory() as client:
        yield client
