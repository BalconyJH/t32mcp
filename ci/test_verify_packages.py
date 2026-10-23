# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Hardware-free checks for the packaging gate itself; no installs or builds."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import verify_packages
from verify_packages import assert_python_rejected, clean_environment, only_archive


class PackagingGateTests(unittest.TestCase):
    def test_removes_hardware_and_stale_artifact_environment(self):
        values = {
            "PATH": "keep",
            "LIBCLANG_PATH": "keep too",
            "T32MCP": "old binary",
            "T32MCP_HARDWARE": "1",
            "T32MCP_T32_PORT": "20000",
            "T32MCP_WHEEL": "old wheel",
            "T32MCP_SDIST": "old source",
            "T32MCP_TOKEN": "old token",
            "t32mcp_future_option": "clear case-insensitively",
            "T32_PORT": "20001",
            "UV_ENV_FILE": "unsafe.env",
            "UV_NO_ENV_FILE": "0",
        }
        with patch.dict(os.environ, values, clear=True):
            self.assertEqual(
                clean_environment(),
                {"PATH": "keep", "LIBCLANG_PATH": "keep too", "UV_NO_ENV_FILE": "1"},
            )

    def test_requires_exactly_one_fresh_archive(self):
        with tempfile.TemporaryDirectory(prefix="t32mcp-ci-test-") as temporary:
            directory = Path(temporary).resolve()
            with self.assertRaises(RuntimeError):
                only_archive(directory, "*.whl")
            wheel = directory / "one.whl"
            wheel.touch()
            self.assertEqual(only_archive(directory, "*.whl"), wheel)
            (directory / "two.whl").touch()
            with self.assertRaises(RuntimeError):
                only_archive(directory, "*.whl")

    def test_rejects_success_on_python_313(self):
        with self.assertRaises(RuntimeError):
            assert_python_rejected(subprocess.CompletedProcess([], 0, "", ""))

    def test_does_not_mistake_an_unrelated_failure_for_version_rejection(self):
        with self.assertRaises(RuntimeError):
            assert_python_rejected(
                subprocess.CompletedProcess([], 1, "", "Network is unavailable")
            )

    def test_accepts_only_the_requires_python_failure(self):
        assert_python_rejected(
            subprocess.CompletedProcess(
                [], 1, "", "Python 3.13 does not satisfy Python>=3.14"
            )
        )

    def test_full_gate_selects_fresh_archives_and_the_installed_binary(self):
        calls = []
        with tempfile.TemporaryDirectory(prefix="t32mcp-ci-flow-") as temporary:
            directory = Path(temporary).resolve()
            scripts = "Scripts" if os.name == "nt" else "bin"
            python_name = "python.exe" if os.name == "nt" else "python"
            binary_name = "t32mcp.exe" if os.name == "nt" else "t32mcp"
            binary = directory / "python-3.14" / scripts / binary_name
            wheel = directory / "archives" / "fresh.whl"
            interpreters = [
                directory / f"python-{version}" / scripts / python_name
                for version in ("3.13", "3.14")
            ]

            def fake_run(arguments, **options):
                calls.append((tuple(arguments), dict(options["env"])))
                if arguments[:2] == ("uv", "build"):
                    archives = directory / "archives"
                    archives.mkdir()
                    (archives / "fresh.whl").touch()
                    (archives / "fresh.tar.gz").touch()
                elif arguments[:2] == ("uv", "venv"):
                    self.assertIn("--no-project", arguments)
                    executable = Path(arguments[-1]) / scripts / python_name
                    executable.parent.mkdir(parents=True, exist_ok=True)
                    executable.touch()
                elif list(arguments[:3]) == ["uv", "pip", "install"]:
                    for option in ("--offline", "--no-index", "--no-deps"):
                        self.assertIn(option, arguments)
                    interpreter = Path(arguments[arguments.index("--python") + 1])
                    self.assertTrue(interpreter.is_file())
                    self.assertIn(interpreter, interpreters)
                    self.assertEqual(arguments[-1], str(wheel))
                    self.assertFalse(binary.exists())
                    if interpreter == interpreters[0]:
                        return subprocess.CompletedProcess(
                            arguments, 1, "", "Python 3.13 needs Python>=3.14"
                        )
                    binary.touch()
                return subprocess.CompletedProcess(arguments, 0, "", "")

            with (
                patch("verify_packages.tempfile.mkdtemp", return_value=temporary),
                patch("verify_packages.subprocess.run", side_effect=fake_run),
                patch("builtins.print"),
                patch.dict(
                    os.environ,
                    {
                        "T32MCP_HARDWARE": "1",
                        "T32MCP": "old",
                        "UV_ENV_FILE": "unsafe.env",
                        "UV_NO_ENV_FILE": "0",
                    },
                ),
            ):
                verify_packages.main()
            installs = [
                arguments
                for arguments, _ in calls
                if arguments[:3] == ("uv", "pip", "install")
            ]
            self.assertEqual(
                [
                    Path(arguments[arguments.index("--python") + 1])
                    for arguments in installs
                ],
                interpreters,
            )
            self.assertEqual([arguments[-1] for arguments in installs], [str(wheel)] * 2)
            arguments, environment = calls[-1]
            self.assertEqual(
                arguments,
                (
                    "uv", "run", "--no-env-file", "--project", "tests",
                    "--locked", "--offline", "pytest", "tests",
                ),
            )
            self.assertEqual(environment["T32MCP"], str(binary))
            self.assertEqual(
                environment["T32MCP_WHEEL"], str(directory / "archives" / "fresh.whl")
            )
            self.assertEqual(
                environment["T32MCP_SDIST"], str(directory / "archives" / "fresh.tar.gz")
            )
            self.assertTrue(
                all("T32MCP_HARDWARE" not in environment for _, environment in calls)
            )
            self.assertTrue(
                all("UV_ENV_FILE" not in environment for _, environment in calls)
            )
            self.assertTrue(
                all(environment["UV_NO_ENV_FILE"] == "1" for _, environment in calls)
            )


if __name__ == "__main__":
    unittest.main()
