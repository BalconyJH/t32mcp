# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""CI contracts and mocked shell flows; never build or contact a debugger.

Run with the locked tests environment, which supplies PyYAML. Shell fixtures
replace /dev/tcp with a fake probe before execution and put fake cargo and a
fake simulator first on PATH. A changed socket boundary fails before execution.
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

try:
    import yaml
except ImportError:
    yaml = None

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_PYTHON_IMAGE = "ghcr.io/astral-sh/uv:python3.14-bookworm-slim"


def bash_executable() -> str | None:
    if os.name == "nt":
        # System32/bash.exe is WSL, not the Git Bash used by these helpers.
        for directory in ("ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"):
            candidate = Path(os.environ.get(directory, "C:/Program Files")) / "Git/bin/bash.exe"
            if candidate.is_file():
                return str(candidate)
        git = shutil.which("git")
        if git:
            candidate = Path(git).resolve().parents[1] / "bin/bash.exe"
            if candidate.is_file():
                return str(candidate)
        return None
    return shutil.which("bash")


@unittest.skipIf(yaml is None, "Use the locked tests environment for PyYAML structural checks")
class ConfigurationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with (ROOT / ".gitlab-ci.yml").open(encoding="utf-8") as stream:
            documents = list(yaml.safe_load_all(stream))
        if len(documents) != 2:
            raise AssertionError("GitLab CI must retain its inputs and pipeline documents")
        cls.inputs = documents[0]["spec"]["inputs"]
        cls.pipeline = documents[1]
        with (ROOT / ".github/workflows/ci.yml").open(encoding="utf-8") as stream:
            cls.github = yaml.safe_load(stream)
        with (ROOT / ".pre-commit-config.yaml").open(encoding="utf-8") as stream:
            cls.hooks = yaml.safe_load(stream)

    def effective_job(self, name):
        job = self.pipeline[name]
        if "extends" not in job:
            return dict(job)
        parent = self.effective_job(job["extends"])
        parent.update(job)
        return parent

    def test_inputs_caches_and_junit_reports_are_preserved(self):
        self.assertEqual(self.inputs["release_version"]["default"], "")
        self.assertEqual(self.inputs["release_dry_run"]["default"], False)
        cache = self.pipeline[".cargo-cache"]
        self.assertEqual(
            set(cache["key"]["files"]),
            {"t32mcp-rs/Cargo.lock", "t32mcp-rs/rust-toolchain.toml"},
        )
        self.assertEqual(cache["policy"], "$CARGO_CACHE_POLICY")
        for name, report in (
            ("test", "t32mcp-rs/target/nextest/ci/junit.xml"),
            ("test:integration", "tests/report.xml"),
        ):
            with self.subTest(job=name):
                artifacts = self.pipeline[name]["artifacts"]
                self.assertEqual(artifacts["reports"]["junit"], report)
                self.assertEqual(artifacts["when"], "always")

    def test_python_314_public_default_is_locked_and_offline_by_default(self):
        variables = self.pipeline["variables"]
        self.assertEqual(variables["PYTEST_IMAGE"], PUBLIC_PYTHON_IMAGE)
        self.assertEqual(variables["UV_NO_ENV_FILE"], "1")
        job = self.effective_job("test:integration")
        self.assertEqual(job["image"], "$PYTEST_IMAGE")
        self.assertEqual(job["before_script"], [])
        for name in ("T32MCP_HARDWARE", "T32MCP_T32_PORT"):
            self.assertFalse(variables.get(name))
            self.assertFalse(job.get("variables", {}).get(name))
        script = "\n".join(job["script"])
        self.assertIn("uv run --no-env-file --locked pytest", script)
        self.assertIn("ARG PYTHON_VERSION=3.14\n", (ROOT / "ci/Dockerfile").read_text())

    def test_native_jobs_have_bindgen_dependencies_and_explicit_simulator_opt_in(self):
        self.assertIn("1.96.0", self.pipeline["variables"]["RUST_TEST_IMAGE"])
        self.assertIn("1.96.0", self.pipeline["variables"]["RUST_IMAGE"])
        for name in (
            "clippy", "test", "check:cross", "build:linux", "build:linux-arm64", "build:windows"
        ):
            with self.subTest(job=name):
                job = self.effective_job(name)
                setup = "\n".join(job["before_script"])
                self.assertIn("clang libclang-dev", setup)
                for command in job["script"]:
                    if command.startswith("cargo "):
                        self.assertIn("--locked", command)
        setup = "\n".join(self.pipeline["build:windows"]["before_script"])
        self.assertIn("llvm lld", setup)
        self.assertEqual(
            self.pipeline["test"]["variables"],
            {"T32MCP_HARDWARE": "1", "T32MCP_T32_PORT": "20000"},
        )
        self.assertIn("ci/start_sim.sh", self.pipeline["test"]["before_script"])
        dockerfile = (ROOT / "ci/Dockerfile").read_text()
        self.assertIn("ARG RUST_VERSION=1.96.0", dockerfile)
        self.assertIn("clang libclang-dev llvm lld", dockerfile)

    def test_release_dependencies_uploads_and_assets_match_supported_native_libraries(self):
        builds = {name for name in self.pipeline if name.startswith("build:")}
        self.assertEqual(builds, {"build:linux", "build:linux-arm64", "build:windows"})
        expected = builds | {"package:skill"}
        for name in ("release:dry-run", "upload"):
            self.assertEqual(set(self.pipeline[name]["needs"]), expected)
        artifacts = {
            "t32mcp-linux-x86_64", "t32mcp-linux-aarch64", "t32mcp-windows-x86_64.exe", "skills.zip"
        }
        links = self.pipeline["release"]["release"]["assets"]["links"]
        self.assertEqual({link["name"] for link in links}, artifacts)
        for name in ("release:dry-run", "upload"):
            script = "\n".join(self.pipeline[name]["script"])
            for artifact in artifacts:
                self.assertIn(f"dist/{artifact}", script)
            self.assertNotIn("macos", script)
        targets = {"aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc"}
        cross = "\n".join(self.pipeline["check:cross"]["script"])
        self.assertEqual(
            set(re.findall(r"cargo (?:xwin )?check --locked --target (\S+)", cross)),
            targets,
        )
        self.assertIn("cargo xwin check --locked --target x86_64-pc-windows-msvc", cross)
        self.assertNotIn("apple-darwin", cross)
        for target in targets | {"x86_64-unknown-linux-gnu"}:
            self.assertTrue((ROOT / "t32mcp-rs/lib" / target).is_dir(), target)

    def test_github_and_hooks_keep_offline_defaults(self):
        environment = self.github["jobs"]["native"]["env"]
        for name in ("T32MCP_HARDWARE", "T32MCP_T32_PORT", "T32SYS", "T32_PORT"):
            self.assertEqual(environment[name], "")
        self.assertEqual(environment["UV_NO_ENV_FILE"], "1")
        hooks = {
            hook["id"]: hook
            for repository in self.hooks["repos"]
            for hook in repository["hooks"]
        }
        self.assertEqual(hooks["cargo-test"]["entry"], "ci/test_rust.sh")
        self.assertIn("--locked", hooks["test"]["entry"])
        self.assertIn("--no-env-file", hooks["test"]["entry"])


class MockedShellTests(unittest.TestCase):
    def setUp(self):
        self.bash = bash_executable()
        if not self.bash:
            self.skipTest("bash is unavailable")
        temporary = tempfile.TemporaryDirectory(prefix="t32mcp-ci-shell-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name).resolve()
        (self.directory / "ci").mkdir()
        (self.directory / "tests").mkdir()
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        self.log = self.directory / "calls.log"
        self.state = self.directory / "simulator-ready"
        self.pid = self.directory / "simulator.pid"
        self.installation = self.directory / "t32"
        simulator = self.installation / "bin/pc_linux64/t32mriscv"
        simulator.parent.mkdir(parents=True)
        for name in ("start_sim.sh", "test_rust.sh"):
            source = (ROOT / "ci" / name).read_text()
            boundary = '(: > "/dev/tcp/127.0.0.1/$T32MCP_T32_PORT") 2> /dev/null'
            self.assertEqual(source.count(boundary), 1)
            source = source.replace(boundary, "ci_test_probe")
            # Mock only the timing and socket boundary; exercise the real flow.
            source = source.replace("attempt<150", "attempt<12")
            source = source.replace("sleep 0.2", "sleep 0.01")
            source = source.replace("sleep 0.1", "sleep 0.01")
            self.assertNotIn("/dev/tcp", source)
            self.write_script(self.directory / "ci" / name, source)
        (self.directory / "tests/sim.t32").touch()
        self.write_script(
            self.bin / "cargo",
            'printf "cargo:%s\\n" "$*" >> "$CI_TEST_LOG"\nexit "${CI_TEST_CARGO_EXIT:-0}"\n',
        )
        self.write_script(
            self.bin / "uname",
            'case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 2 ;; esac\n',
        )
        self.write_script(
            self.bin / "ci_test_probe",
            'printf "probe:%s\\n" "$T32MCP_T32_PORT" >> "$CI_TEST_LOG"\n'
            '[[ "${CI_TEST_OCCUPIED:-}" == 1 || -f "$CI_TEST_STATE" ]]\n',
        )
        self.write_script(
            simulator,
            'printf "simulator:%s\\n" "$*" >> "$CI_TEST_LOG"\n'
            'cleanup() { rm -f "$CI_TEST_STATE"; printf "simulator-stopped\\n" >> "$CI_TEST_LOG"; }\n'
            'trap cleanup EXIT\ntrap "exit 0" TERM INT\n'
            '[[ "${CI_TEST_SIM_EXIT:-}" != 1 ]] || exit 17\n'
            'printf "%s" "$$" > "$CI_TEST_PID"\n'
            'if [[ "${CI_TEST_NO_READY:-}" != 1 ]]; then : > "$CI_TEST_STATE"; fi\n'
            'while :; do sleep 0.01; done\n',
        )
        # Observe all signals, while allowing them only to the fake process
        # created by these scripts. A live-debugger path must never signal.
        self.bash_env = self.directory / "bash-env"
        self.bash_env.write_text(
            'kill() { printf "kill:%s\\n" "$*" >> "$CI_TEST_LOG"; builtin kill "$@"; }\n'
            'uname() { case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; *) return 2 ;; esac; }\n',
            encoding="utf-8",
            newline="\n",
        )

    @staticmethod
    def write_script(path, source):
        path.write_text("#!/usr/bin/env bash\n" + source, encoding="utf-8", newline="\n")
        path.chmod(0o755)

    def run_script(self, script, **values):
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.upper().startswith(("T32", "CI_TEST_"))
            and key.upper() not in {"BASH_ENV", "ENV", "CDPATH"}
        }
        environment.update(
            PATH=str(self.bin) + os.pathsep + os.environ.get("PATH", ""),
            TMPDIR=self.directory.as_posix(),
            BASH_ENV=self.bash_env.as_posix(),
            CI_TEST_LOG=self.log.as_posix(),
            CI_TEST_STATE=self.state.as_posix(),
            CI_TEST_PID=self.pid.as_posix(),
        )
        environment.update(values)
        arguments = [self.bash, "--noprofile", "--norc", f"ci/{script}"]
        if script == "start_sim.sh":
            arguments.append("requested.pid")
        result = subprocess.run(
            arguments, cwd=self.directory, env=environment,
            text=True, capture_output=True, timeout=15, check=False,
        )
        calls = self.log.read_text().splitlines() if self.log.exists() else []
        return result, calls

    def assert_unit_tests_only(self, calls):
        self.assertEqual(calls, ["cargo:test --manifest-path t32mcp-rs/Cargo.toml --locked --lib --bins"])

    def test_default_and_partial_opt_ins_never_probe_or_spawn(self):
        for values in (
            {}, {"T32SYS": self.installation.as_posix()},
            {"T32MCP_T32_PORT": "23000"}, {"T32MCP_HARDWARE": "1"},
            {"T32SYS": self.installation.as_posix(), "T32MCP_HARDWARE": "1"},
            {"T32SYS": self.installation.as_posix(), "T32MCP_T32_PORT": "23000"},
            {"T32MCP_HARDWARE": "true", "T32MCP_T32_PORT": "23000"},
        ):
            with self.subTest(values=values):
                self.log.unlink(missing_ok=True)
                result, calls = self.run_script("test_rust.sh", **values)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assert_unit_tests_only(calls)

    def test_invalid_explicit_ports_fail_before_io_in_both_helpers(self):
        for script in ("test_rust.sh", "start_sim.sh"):
            for port in ("0", "65536", "-1", "word", "1 2", "123456", "1;exit", "00000"):
                with self.subTest(script=script, port=port):
                    self.log.unlink(missing_ok=True)
                    result, calls = self.run_script(
                        script, T32MCP_HARDWARE="1", T32MCP_T32_PORT=port,
                        T32SYS=self.installation.as_posix(),
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("integer from 1 to 65535", result.stderr)
                    self.assertEqual(calls, [])

    def test_start_sim_requires_both_opt_ins_before_io(self):
        for values in (
            {}, {"T32MCP_HARDWARE": "1"}, {"T32MCP_T32_PORT": "23000"},
            {"T32MCP_HARDWARE": "yes", "T32MCP_T32_PORT": "23000"},
        ):
            with self.subTest(values=values):
                result, calls = self.run_script(
                    "start_sim.sh", T32SYS=self.installation.as_posix(), **values,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])

    def test_explicit_existing_debugger_runs_serially_without_start_or_cleanup(self):
        result, calls = self.run_script(
            "test_rust.sh", T32MCP_HARDWARE="1", T32MCP_T32_PORT="23000",
            T32SYS=self.installation.as_posix(), CI_TEST_OCCUPIED="1",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [
            "probe:23000",
            "cargo:test --manifest-path t32mcp-rs/Cargo.toml --locked -- --test-threads=1",
        ])
        self.assertFalse(self.pid.exists())

    def test_authorized_missing_debugger_without_installation_runs_units(self):
        result, calls = self.run_script(
            "test_rust.sh", T32MCP_HARDWARE="1", T32MCP_T32_PORT="23000",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0], "probe:23000")
        self.assert_unit_tests_only(calls[1:])

    def test_start_sim_refuses_an_occupied_port_without_spawning_or_signalling(self):
        result, calls = self.run_script(
            "start_sim.sh", T32MCP_HARDWARE="1", T32MCP_T32_PORT="23000",
            T32SYS=self.installation.as_posix(), CI_TEST_OCCUPIED="1",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("occupied", result.stderr)
        self.assertEqual(calls, ["probe:23000"])
        self.assertFalse((self.directory / "requested.pid").exists())

    def test_owns_and_cleans_up_only_its_simulator_on_success_and_cargo_failure(self):
        for status in ("0", "19"):
            with self.subTest(cargo_exit=status):
                self.log.unlink(missing_ok=True)
                result, calls = self.run_script(
                    "test_rust.sh", T32MCP_HARDWARE="1", T32MCP_T32_PORT="02300",
                    T32SYS=self.installation.as_posix(), CI_TEST_CARGO_EXIT=status,
                )
                self.assertEqual(result.returncode, int(status), result.stderr)
                self.assertTrue(any(call.startswith("simulator:") for call in calls), calls)
                self.assertTrue(any(call.startswith("cargo:") for call in calls), calls)
                self.assertIn("simulator-stopped", calls)
                pid = self.pid.read_text()
                signals = [call for call in calls if call.startswith("kill:")]
                self.assertTrue(signals)
                self.assertTrue(
                    all(call.partition(":")[2].rsplit(" ", 1)[-1] == pid for call in signals),
                    signals,
                )
                self.assertIn("probe:2300", calls)
                self.assertFalse(self.state.exists())

    def test_failed_start_and_timeout_clean_up_without_publishing_a_pid(self):
        for values in ({"CI_TEST_SIM_EXIT": "1"}, {"CI_TEST_NO_READY": "1"}):
            with self.subTest(values=values):
                self.log.unlink(missing_ok=True)
                result, calls = self.run_script(
                    "start_sim.sh", T32MCP_HARDWARE="1", T32MCP_T32_PORT="23000",
                    T32SYS=self.installation.as_posix(), **values,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(any(call.startswith("simulator:") for call in calls), calls)
                self.assertIn("simulator-stopped", calls)
                self.assertFalse((self.directory / "requested.pid").exists())
                self.assertFalse(any(call.startswith("cargo:") for call in calls))
                self.assertFalse(self.state.exists())


if __name__ == "__main__":
    unittest.main()
