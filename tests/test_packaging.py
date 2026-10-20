# SPDX-FileCopyrightText: 2026 Lauterbach GmbH
# SPDX-License-Identifier: Apache-2.0

"""Hardware-free checks; run directly to avoid the integration-test fixtures."""

import email
import os
import tarfile
import tomllib
import unittest
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LICENSE_FILES = {
    "LICENSES/Apache-2.0.txt",
    "LICENSES/LicenseRef-Lauterbach-RemoteAPI.txt",
}


class PackagingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with (ROOT / "pyproject.toml").open("rb") as stream:
            cls.config = tomllib.load(stream)
        with (ROOT / "t32mcp-rs" / "Cargo.toml").open("rb") as stream:
            cls.cargo = tomllib.load(stream)

    def test_native_binary_backend_and_version(self):
        self.assertEqual(self.config["build-system"]["build-backend"], "maturin")
        self.assertEqual(self.config["project"]["name"], self.cargo["package"]["name"])
        self.assertIn("version", self.config["project"]["dynamic"])
        self.assertNotIn("version", self.config["project"])
        self.assertNotIn("scripts", self.config["project"])
        self.assertEqual(
            self.config["tool"]["maturin"]["manifest-path"],
            "t32mcp-rs/Cargo.toml",
        )
        self.assertEqual(self.config["tool"]["maturin"]["bindings"], "bin")
        self.assertTrue(self.config["tool"]["maturin"]["locked"])

    def test_license_metadata(self):
        self.assertEqual(
            self.config["project"]["license"],
            "Apache-2.0 AND LicenseRef-Lauterbach-RemoteAPI",
        )
        self.assertEqual(set(self.config["project"]["license-files"]), LICENSE_FILES)
        for path in LICENSE_FILES:
            self.assertTrue((ROOT / path).is_file(), path)
        self.assertIn("Private :: Do Not Upload", self.config["project"]["classifiers"])

    @unittest.skipUnless(os.environ.get("T32MCP_WHEEL"), "T32MCP_WHEEL is not set")
    def test_wheel_contents(self):
        with zipfile.ZipFile(os.environ["T32MCP_WHEEL"]) as archive:
            names = set(archive.namelist())
            metadata_paths = [
                name for name in names if name.endswith(".dist-info/METADATA")
            ]
            self.assertEqual(len(metadata_paths), 1)
            metadata = email.message_from_bytes(archive.read(metadata_paths[0]))
            self.assertEqual(metadata["Name"], self.config["project"]["name"])
            self.assertEqual(metadata["Version"], self.cargo["package"]["version"])
            self.assertEqual(
                metadata["License-Expression"], self.config["project"]["license"]
            )
            self.assertEqual(set(metadata.get_all("License-File", [])), LICENSE_FILES)
            binaries = [
                name
                for name in names
                if name.endswith((".data/scripts/t32mcp", ".data/scripts/t32mcp.exe"))
            ]
            self.assertEqual(len(binaries), 1, names)
            for path in LICENSE_FILES:
                self.assertTrue(
                    any(name.endswith(f".dist-info/licenses/{path}") for name in names),
                    path,
                )
            self.assertFalse(any("/skill-trace32/" in name for name in names))
            self.assertFalse(any("/skills/" in name for name in names))

    @unittest.skipUnless(os.environ.get("T32MCP_SDIST"), "T32MCP_SDIST is not set")
    def test_sdist_contents(self):
        with tarfile.open(os.environ["T32MCP_SDIST"], "r:*") as archive:
            names = {name.partition("/")[2] for name in archive.getnames()}
        required = LICENSE_FILES | {
            "pyproject.toml",
            "README.md",
            "docs/installation.md",
            "docs/local-capabilities.md",
            "docs/local-verification.md",
            "docs/evidence-workflows.md",
            "docs/workflow-verification.md",
            "docs/peripheral-inspection.md",
            "docs/peripheral-verification.md",
            "t32mcp-rs/Cargo.toml",
            "t32mcp-rs/Cargo.lock",
            "t32mcp-rs/rust-toolchain.toml",
            "t32mcp-rs/build.rs",
            "t32mcp-rs/src/main.rs",
            "t32mcp-rs/src/lib.rs",
            "t32mcp-rs/src/t32.rs",
            "t32mcp-rs/src/t32api.rs",
            "t32mcp-rs/src/mcp.rs",
            "t32mcp-rs/src/execution_policy.rs",
            "t32mcp-rs/src/docs.rs",
            "t32mcp-rs/src/tools/mod.rs",
            "t32mcp-rs/src/tools/native.rs",
            "t32mcp-rs/src/tools/trace32_run_script.rs",
            "t32mcp-rs/src/tools/trace32_run_command.rs",
            "t32mcp-rs/src/tools/trace32_get_status.rs",
            "t32mcp-rs/src/tools/trace32_manage_instances.rs",
            "t32mcp-rs/mcp_2025_03_26.json",
            "t32mcp-rs/mcp_2025_11_25.json",
            "t32mcp-rs/mcp_2026_07_28.json",
            "t32mcp-rs/src/windows_paths.rs",
            "t32mcp-rs/src/artifacts.rs",
            "t32mcp-rs/src/capabilities.rs",
            "t32mcp-rs/src/catalog.rs",
            "t32mcp-rs/src/diagnostics.rs",
            "t32mcp-rs/src/image_verify.rs",
            "t32mcp-rs/src/inspection.rs",
            "t32mcp-rs/src/snapshot.rs",
            "t32mcp-rs/src/peripherals.rs",
            "t32mcp-rs/src/peripheral_views.rs",
            "t32mcp-rs/lib/t32.h",
            "t32mcp-rs/lib/x86_64-pc-windows-msvc/t32api64.lib",
            "t32mcp-rs/lib/x86_64-unknown-linux-gnu/libt32api64.a",
            "t32mcp-rs/lib/aarch64-unknown-linux-gnu/libt32api64.a",
        }
        self.assertTrue(required <= names, sorted(required - names))
        local_directories = {"target", ".idea", ".venv", "__pycache__", "dist"}
        self.assertFalse(
            any(local_directories.intersection(name.split("/")) for name in names)
        )
        self.assertFalse(any(name.endswith((".pyc", ".pyo")) for name in names))

    @unittest.skipUnless(os.environ.get("T32MCP_SDIST"), "T32MCP_SDIST is not set")
    def test_sdist_contains_current_sources_and_guides(self):
        with tarfile.open(os.environ["T32MCP_SDIST"], "r:*") as archive:
            members = {
                member.name.partition("/")[2]: member
                for member in archive.getmembers()
                if member.isfile()
            }
            sources = [
                *(ROOT / "t32mcp-rs" / "src").rglob("*.rs"),
                *(ROOT / "t32mcp-rs" / "src" / "docs").glob("*.md"),
                *(ROOT / "t32mcp-rs").glob("mcp_*.json"),
                *(ROOT / "docs").glob("*.md"),
            ]
            for source in sources:
                relative = source.relative_to(ROOT).as_posix()
                self.assertIn(relative, members)
                with archive.extractfile(members[relative]) as stream:
                    self.assertEqual(stream.read(), source.read_bytes(), relative)


if __name__ == "__main__":
    unittest.main()
