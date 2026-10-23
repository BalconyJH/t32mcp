# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Build fresh archives and test an isolated wheel without TRACE32 hardware."""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def clean_environment() -> dict[str, str]:
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.upper().startswith("T32MCP")
        and key.upper() not in {"T32_PORT", "UV_ENV_FILE", "UV_NO_ENV_FILE"}
    }
    environment["UV_NO_ENV_FILE"] = "1"
    return environment


def only_archive(directory: Path, pattern: str) -> Path:
    archives = list(directory.glob(pattern))
    if len(archives) != 1:
        raise RuntimeError(f"Expected exactly one {pattern} in {directory}: {archives}")
    return archives[0].resolve()


def assert_python_rejected(result: subprocess.CompletedProcess) -> None:
    if result.returncode == 0:
        raise RuntimeError("The wheel unexpectedly installed with Python 3.13")
    diagnostic = (result.stdout + result.stderr).lower().replace(" ", "")
    if "python>=3.14" not in diagnostic:
        raise RuntimeError(
            "Python 3.13 failed for a reason other than Requires-Python:\n"
            + result.stdout
            + result.stderr
        )


def main() -> None:
    env = clean_environment()
    work = Path(
        tempfile.mkdtemp(prefix="t32mcp-packages-", dir=os.environ.get("RUNNER_TEMP"))
    ).resolve()
    archives = work / "archives"
    print(f"Fresh packaging workspace: {work}", flush=True)

    def run(*arguments: str, **options) -> subprocess.CompletedProcess:
        print("+ " + " ".join(map(str, arguments)), flush=True)
        return subprocess.run(arguments, cwd=ROOT, env=env, check=True, **options)

    # With neither --sdist nor --wheel, uv builds a wheel FROM the fresh sdist.
    run("uv", "build", "--python", "3.14", "--out-dir", str(archives))
    env["T32MCP_WHEEL"] = str(only_archive(archives, "*.whl"))
    env["T32MCP_SDIST"] = str(only_archive(archives, "*.tar.gz"))
    run(sys.executable, "tests/test_packaging.py")

    environments = {}
    for version in ("3.13", "3.14"):
        directory = work / f"python-{version}"
        run("uv", "venv", "--no-project", "--python", version, str(directory))
        scripts = directory / ("Scripts" if os.name == "nt" else "bin")
        environments[version] = scripts

    python_name = "python.exe" if os.name == "nt" else "python"
    reject_python = environments["3.13"] / python_name
    rejected = subprocess.run(
        [
            "uv", "pip", "install", "--python", str(reject_python),
            "--no-deps", "--no-index", "--offline", env["T32MCP_WHEEL"],
        ],
        cwd=ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    print(rejected.stdout + rejected.stderr, flush=True)
    assert_python_rejected(rejected)

    accept_python = environments["3.14"] / python_name
    run(
        "uv", "pip", "install", "--python", str(accept_python),
        "--no-deps", "--no-index", "--offline", env["T32MCP_WHEEL"],
    )
    binary_name = "t32mcp.exe" if os.name == "nt" else "t32mcp"
    binary = environments["3.14"] / binary_name
    if not binary.is_file():
        raise RuntimeError(f"The installed wheel did not provide {binary}")
    env["T32MCP"] = str(binary)
    run(
        "uv", "run", "--no-env-file", "--project", "tests",
        "--locked", "--offline", "pytest", "tests",
    )


if __name__ == "__main__":
    main()
