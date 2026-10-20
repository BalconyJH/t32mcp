# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Collection must not discover or contact a connected hardware session."""

import runpy
import socket
from pathlib import Path

import pytest


@pytest.mark.parametrize(
    ("enabled", "port"),
    [(None, None), (None, "20000"), ("0", "20000"), ("1", None)],
)
def test_contract_collection_requires_both_explicit_opt_ins(monkeypatch, enabled, port):
    for name, value in (("T32MCP_HARDWARE", enabled), ("T32MCP_T32_PORT", port)):
        if value is None:
            monkeypatch.delenv(name, raising=False)
        else:
            monkeypatch.setenv(name, value)

    def forbidden_socket(*_args, **_kwargs):
        pytest.fail("contract collection must not create a debugger socket")

    monkeypatch.setattr(socket, "socket", forbidden_socket)
    module = runpy.run_path(str(Path(__file__).with_name("test_contract.py")))
    assert module["pytestmark"].name == "skipif"
    assert module["pytestmark"].args == (True,)


def test_enabled_contract_collection_still_does_not_probe(monkeypatch):
    monkeypatch.setenv("T32MCP_HARDWARE", "1")
    monkeypatch.setenv("T32MCP_T32_PORT", "20000")

    def forbidden_socket(*_args, **_kwargs):
        pytest.fail("hardware probing belongs in the opted-in test fixture")

    monkeypatch.setattr(socket, "socket", forbidden_socket)
    module = runpy.run_path(str(Path(__file__).with_name("test_contract.py")))
    assert module["pytestmark"].args == (False,)
