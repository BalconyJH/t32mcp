# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Simulator fixture safety and lifecycle checks without a real debugger."""

import os
import subprocess
from types import SimpleNamespace
from unittest.mock import Mock, call

import pytest

import conftest


def forbid_io(*_args, **_kwargs):
    pytest.fail("fixture must not access a debugger or start a process")


def set_opt_ins(monkeypatch, enabled, port, t32sys=None):
    for name, value in (
        ("T32MCP_HARDWARE", enabled),
        ("T32MCP_T32_PORT", port),
        ("T32SYS", t32sys),
    ):
        if value is None:
            monkeypatch.delenv(name, raising=False)
        else:
            monkeypatch.setenv(name, value)


def forbid_debugger_io(monkeypatch):
    monkeypatch.setattr(conftest.socket, "socket", forbid_io)
    monkeypatch.setattr(conftest.socket, "create_connection", forbid_io)
    monkeypatch.setattr(conftest.subprocess, "Popen", forbid_io)


@pytest.mark.parametrize("t32sys", [None, "unused-trace32-installation"])
@pytest.mark.parametrize(
    ("enabled", "port"),
    [
        (enabled, port)
        for enabled in (None, "0", "1")
        for port in (None, "", "31234", "invalid")
        if enabled != "1" or not port
    ],
)
def test_powerview_requires_both_explicit_opt_ins(monkeypatch, enabled, port, t32sys):
    set_opt_ins(monkeypatch, enabled, port, t32sys)
    forbid_debugger_io(monkeypatch)
    factory = SimpleNamespace(mktemp=forbid_io)

    with pytest.raises(pytest.skip.Exception, match="require T32MCP_HARDWARE=1"):
        next(conftest.powerview.__wrapped__(factory))


@pytest.mark.parametrize("port", ["invalid", " ", "0", "-1", "65536", "20000.5"])
def test_invalid_port_fails_before_io(monkeypatch, port):
    set_opt_ins(monkeypatch, "1", port, "unused-trace32-installation")
    forbid_debugger_io(monkeypatch)
    factory = SimpleNamespace(mktemp=forbid_io)

    with pytest.raises(pytest.fail.Exception, match="invalid T32MCP_T32_PORT"):
        next(conftest.powerview.__wrapped__(factory))


@pytest.mark.parametrize(
    ("system", "machine", "directory", "filename"),
    [
        ("win32", "AMD64", "windows64", "t32mriscv.exe"),
        ("darwin", "arm64", "macosx64", "t32mriscv-qt"),
        ("linux", "x86_64", "pc_linux64", "t32mriscv"),
        ("linux", "aarch64", "linux-aarch64", "t32mriscv"),
        ("linux", "ARM64", "linux-aarch64", "t32mriscv"),
    ],
)
def test_powerview_executable_platform_path(monkeypatch, system, machine, directory, filename):
    monkeypatch.setattr(conftest.sys, "platform", system)
    monkeypatch.setattr(conftest.platform, "machine", lambda: machine)

    assert conftest.powerview_executable("trace32") == os.path.join(
        "trace32", "bin", directory, filename
    )


def test_reachable_approved_endpoint_is_never_owned(monkeypatch):
    set_opt_ins(monkeypatch, "1", "31234", "unused-trace32-installation")
    reachable = Mock(return_value=True)
    monkeypatch.setattr(conftest, "powerview_reachable", reachable)
    monkeypatch.setattr(conftest.subprocess, "Popen", forbid_io)
    monkeypatch.setattr(conftest, "_stop_owned_process", forbid_io)
    fixture = conftest.powerview.__wrapped__(SimpleNamespace(mktemp=forbid_io))

    assert next(fixture) == 31234
    reachable.assert_called_once_with(31234)
    with pytest.raises(StopIteration):
        next(fixture)


def test_unreachable_endpoint_without_t32sys_skips(monkeypatch):
    set_opt_ins(monkeypatch, "1", "31234")
    reachable = Mock(return_value=False)
    monkeypatch.setattr(conftest, "powerview_reachable", reachable)
    monkeypatch.setattr(conftest.subprocess, "Popen", forbid_io)

    with pytest.raises(pytest.skip.Exception, match="explicitly selected port 31234"):
        next(conftest.powerview.__wrapped__(SimpleNamespace(mktemp=forbid_io)))
    reachable.assert_called_once_with(31234)


@pytest.mark.parametrize("system", ["win32", "linux"])
@pytest.mark.parametrize("startup_fails", [False, True])
def test_owned_simulator_startup_and_cleanup(monkeypatch, tmp_path, system, startup_fails):
    set_opt_ins(monkeypatch, "1", "031234", "trace32-installation")
    monkeypatch.setenv("T32MCP_FIXTURE_SENTINEL", "preserved")
    monkeypatch.setattr(conftest.sys, "platform", system)
    monkeypatch.setattr(conftest.subprocess, "CREATE_NO_WINDOW", 0x08000000, raising=False)
    monkeypatch.setattr(conftest, "powerview_reachable", Mock(return_value=False))
    process = Mock()
    process.poll.return_value = None
    popen = Mock(return_value=process)
    monkeypatch.setattr(conftest.subprocess, "Popen", popen)
    wait = Mock(side_effect=TimeoutError("startup timed out") if startup_fails else None)
    monkeypatch.setattr(conftest, "wait_for_port", wait)
    fixture = conftest.powerview.__wrapped__(SimpleNamespace(mktemp=lambda _name: tmp_path))

    if startup_fails:
        with pytest.raises(TimeoutError, match="startup timed out"):
            next(fixture)
    else:
        assert next(fixture) == 31234
        process.terminate.assert_not_called()
        assert not popen.call_args.kwargs["stdout"].closed
        fixture.close()

    argv = popen.call_args.args[0]
    options = popen.call_args.kwargs
    assert argv == [conftest.powerview_executable("trace32-installation"), "-c", conftest.POWERVIEW_CONFIG]
    assert options["env"]["T32MCP_T32_PORT"] == "31234"
    assert options["env"]["T32SYS"] == "trace32-installation"
    assert options["env"]["T32MCP_FIXTURE_SENTINEL"] == "preserved"
    assert options["stdout"].name == str(tmp_path / "powerview.log")
    assert options["stdout"].closed
    assert options["stderr"] == subprocess.STDOUT
    if system == "win32":
        assert options["creationflags"] == subprocess.CREATE_NO_WINDOW
    else:
        assert "creationflags" not in options
    wait.assert_called_once_with(31234, process, timeout=30.0)
    process.terminate.assert_called_once_with()
    process.wait.assert_called_once_with(timeout=5)
    process.kill.assert_not_called()


def test_spawn_failure_closes_log_without_process_cleanup(monkeypatch, tmp_path):
    set_opt_ins(monkeypatch, "1", "31234", "trace32-installation")
    monkeypatch.setattr(conftest, "powerview_reachable", Mock(return_value=False))
    popen = Mock(side_effect=OSError("cannot start simulator"))
    monkeypatch.setattr(conftest.subprocess, "Popen", popen)
    monkeypatch.setattr(conftest, "_stop_owned_process", forbid_io)
    fixture = conftest.powerview.__wrapped__(SimpleNamespace(mktemp=lambda _name: tmp_path))

    with pytest.raises(OSError, match="cannot start simulator"):
        next(fixture)
    assert popen.call_args.kwargs["stdout"].closed


def test_owned_process_is_reaped_after_kill():
    process = Mock()
    process.poll.return_value = None
    process.wait.side_effect = [subprocess.TimeoutExpired("simulator", 5), 0]

    conftest._stop_owned_process(process)

    assert process.mock_calls == [
        call.poll(), call.terminate(), call.wait(timeout=5), call.kill(), call.wait(timeout=5)
    ]


def test_already_exited_process_is_not_terminated():
    process = Mock()
    process.poll.return_value = 0

    conftest._stop_owned_process(process)

    process.terminate.assert_not_called()
    process.kill.assert_not_called()
