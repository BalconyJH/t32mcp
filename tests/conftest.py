# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Offline MCP fixtures: never use a developer's active TRACE32 API port."""

import logging
import os
import secrets
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
OFFLINE_NATIVE_TOOLS = {"list_practice_skills", "diff_snapshots"}
DEFAULT_TOOLS = {
    "trace32_get_status",
    "trace32_run_script",
    "trace32_run_command",
    "trace32_manage_instances",
}
READ_ONLY_TOOLS = {"trace32_get_status"}
SCRIPTS_TOOLS = DEFAULT_TOOLS - {"trace32_run_command"}
STRICT_TOOLS = {"trace32_get_status", "trace32_run_script"}


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


def skills_dir(request=None) -> str:
    """The repository's skills/ directory, a valid --skills root."""
    path = ROOT / "skills"
    assert path.is_dir()
    return str(path)


@contextmanager
def reserved_offline_port(port=0):
    """Reserve a localhost endpoint without listening or probing any debugger."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reserved:
        if hasattr(socket, "SO_EXCLUSIVEADDRUSE"):
            reserved.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        reserved.bind(("127.0.0.1", port))
        yield reserved.getsockname()[1]


@pytest.fixture(scope="session")
def offline_port():
    with reserved_offline_port() as port:
        yield port


@pytest.fixture(scope="session")
def second_offline_port():
    with reserved_offline_port() as port:
        yield port


@pytest.fixture(scope="session")
def server_executable() -> str:
    return t32mcp_path()


@pytest.fixture(scope="session")
def skills_base(tmp_path_factory) -> Path:
    base = tmp_path_factory.mktemp("skills with spaces")
    skill = base / "fixture"
    scripts = skill / "scripts"
    scripts.mkdir(parents=True)
    (skill / "SKILL.md").write_text("# Offline fixture\n", encoding="ascii")
    (scripts / "read.cmm").write_text(
        '; {"interface": 1, "strict": true, "arguments": '
        '{"key": {"type": "string", "optional": true}}}\n\nENDDO\n',
        encoding="ascii",
    )
    (scripts / "ignored.txt").write_text("", encoding="ascii")
    return base


@pytest.fixture(scope="session")
def offline_server_args(skills_base, tmp_path_factory, offline_port) -> list[str]:
    # Holding this unlistened port prevents an accidental connection to a board.
    evidence = tmp_path_factory.mktemp("offline evidence")
    return [
        "--execution-policy", "unrestricted",
        "--port", str(offline_port),
        "--skills", str(skills_base),
        "--artifacts", str(evidence),
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


def free_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait_for_port(port: int, process: subprocess.Popen, timeout: float = 10.0):
    """Wait for our HTTP server, never for a TRACE32 Remote API endpoint."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        assert process.poll() is None, f"t32mcp exited with {process.returncode}"
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise TimeoutError(f"t32mcp does not listen on port {port}")


@pytest.fixture(scope="module")
def local_mcp_server(server_executable, offline_server_args) -> Client:
    LOGGER.info("Setting up offline STDIO server: %s", server_executable)
    return Client(StdioTransport(command=server_executable, args=list(offline_server_args)))


@pytest.fixture(scope="module")
def native_mcp_server(server_executable, offline_server_args) -> Client:
    return Client(
        StdioTransport(
            command=server_executable,
            args=[*offline_server_args, "--native-tools"],
        )
    )


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
            if error.code in {400, 401, 405, 406, 415}:
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
        port = free_port()
        token = secrets.token_urlsafe(32)
        url = f"http://127.0.0.1:{port}/mcp"
        argv = [
            server_executable, *offline_server_args, *extra_args,
            "--http", str(port), "--token", token,
        ]
        LOGGER.info("Setting up offline HTTP server on port %s", port)
        with tempfile.TemporaryFile() as log_file:
            process = subprocess.Popen(
                argv, stdin=subprocess.DEVNULL, stdout=log_file, stderr=log_file,
            )
            try:
                _wait_for_http_server(process, url, log_file)
                yield Client(StreamableHttpTransport(url=url, auth=token))
            finally:
                _stop_owned_process(process)
    return factory


@pytest.fixture(scope="module")
def remote_mcp_server(http_mcp_client_factory) -> Client:
    with http_mcp_client_factory() as client:
        yield client
