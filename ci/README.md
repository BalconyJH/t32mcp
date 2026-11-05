# CI

Maintainer notes for `.gitlab-ci.yml` and the helpers in this directory.
Contributor setup is in [CONTRIBUTING.md](../CONTRIBUTING.md).

| File | Purpose |
|---|---|
| `Dockerfile` | Optional Rust and Python 3.14 CI images with the TRACE32 simulator. |
| `start_sim.sh` | Explicitly opted-in simulator startup; refuses an occupied port. |
| `test_rust.sh` | Locked Rust tests, offline by default. |
| `verify_packages.py` | Fresh sdist/wheel checks with hardware and env-file settings removed. |
| `test_verify_packages.py`, `test_ci_integration.py` | Mocked checks of the helpers and CI configuration. |
| `check_version.sh`, `extract_changelog.sh` | Release version and changelog checks. |

## Pipelines

Push pipelines run these checks in the `test` stage:

| Job | Image | What it does |
|---|---|---|
| `clippy` | `RUST_TEST_IMAGE` | Locked clippy checks of all Rust targets and features. |
| `test` | `RUST_TEST_IMAGE` | Builds the debug binary and runs nextest against its own isolated simulator, with explicit hardware opt-in and port 20000. |
| `test:integration` | Public Python 3.14 uv image | Runs the locked Python suite against the debug binary; hardware tests are skipped by default. |
| `check:cross` | `RUST_IMAGE` | Compile-only checks for Linux ARM64 and Windows x64. |
| `sast`, `secret_detection` | GitLab templates | Security scans. |

The Rust and Python test jobs upload JUnit reports for GitLab's **Tests** tab and MR widget.
The native fork needs clang/libclang to generate its vendor API bindings. Existing upstream Rust
image tags do not provide those dependencies, so the jobs install them in `before_script`.
The Windows release job also installs LLVM/lld. `Dockerfile` includes these packages for future
fork images, but editing it does not change the already published upstream tags.

The GitHub **Offline Checks** workflow keeps hardware opt-in and TRACE32 installation settings
empty. It runs native offline checks on Linux x64 and Windows x64, compiles integration tests
without executing them, and checks fresh package archives and the installed wheel.

Release pipelines set the `release_version` input. They skip the push checks and run
`verify:release`, the three native `build:*` jobs, and `package:skill`, followed by either the
dry run or upload and release jobs. Tag pipelines are disabled.

The bundled native vendor libraries support Windows x64, Linux x64, and Linux ARM64.
This fork has no macOS release jobs or macOS release assets. Upstream macOS binaries are not
builds of this native fork. Compile-only cross checks do not establish release linking support;
the release jobs must still link the bundled library for each supported target.
The Windows compile check uses `cargo xwin check` to match the release build's SDK environment.

## Explicit simulator opt-in

Both shell helpers require `T32MCP_HARDWARE=1` **and** an explicit `T32MCP_T32_PORT` before
opening a debugger socket or starting a simulator. The port must contain one to five decimal
digits and be in `1..65535`; invalid nonempty values fail before debugger I/O. There is no default
port. `T32SYS` alone never authorizes a probe or simulator startup.

Without both gates, `test_rust.sh` runs only:

```sh
cargo test --manifest-path t32mcp-rs/Cargo.toml --locked --lib --bins
```

With both gates, it uses a PowerView already listening on the selected localhost port. If the
port is free and `T32SYS` names an installation, it starts its own simulator, runs the tests
serially, and stops only that spawned simulator. If no PowerView is listening and `T32SYS` is
unset, it runs the offline unit tests. `start_sim.sh` always refuses an occupied port. Failed
startup cleans up its own process; successful startup can return its PID through an optional
pidfile. Logs use a unique temporary file and are retained for diagnosis.

An explicit local simulator run is:

```sh
T32MCP_HARDWARE=1 T32MCP_T32_PORT=23000 T32SYS=/path/to/T32 ci/test_rust.sh
```

The simulator configuration is `tests/sim.t32`. Platform paths match the Python fixture:
`bin/pc_linux64/t32mriscv`, `bin/linux-aarch64/t32mriscv`,
`bin/macosx64/t32mriscv-qt`, or `bin/windows64/t32mriscv.exe`.
The shell helpers need Git Bash on Windows. A simulator executable for a platform does not
imply that this fork can build a native MCP binary there.

Python tests follow the same explicit opt-in; see [tests/README.md](../tests/README.md).
Their default GitLab image is the existing public
`ghcr.io/astral-sh/uv:python3.14-bookworm-slim`. No unpublished custom Python image is referenced.
To run the Python simulator tests, first build, test, and publish a Python **3.14** image from
the `pytest` Dockerfile target. Then explicitly override `PYTEST_IMAGE`, `T32MCP_HARDWARE=1`,
and `T32MCP_T32_PORT` in that pipeline. The image supplies `T32SYS=/opt/t32`; the fixture owns
the simulator it starts. Setting a custom image or `T32SYS` alone keeps those tests skipped.
The Python job and pre-commit hook use `--no-env-file --locked`, so a local `.env` cannot opt in.

## Caches

Each Rust job has its own cargo registry and `t32mcp-rs/target/` cache, keyed by job,
`Cargo.lock`, and `rust-toolchain.toml`. `build:windows` also caches `.xwin-cache/`, keyed by
`ci/Dockerfile`. A dependency, toolchain, or image change starts the corresponding cache fresh.
Only default-branch pipelines save caches; other branches restore them. `CARGO_INCREMENTAL=0`
and `CARGO_PROFILE_DEV_DEBUG=line-tables-only` keep development artifacts smaller.

## Releasing

1. Bump `version` in `t32mcp-rs/Cargo.toml` and add its dated section to `CHANGELOG.md`.
2. Optionally run a build-only dry run on a branch:

   ```sh
   glab ci run -b <branch> --input release_version:v0.4.0 --input release_dry_run:true
   ```

3. Release from the default branch:

   ```sh
   glab ci run -b main --input release_version:v0.4.0
   ```

The GitLab **Build → Pipelines → Run pipeline** form exposes the same inputs.
An empty `release_version` creates no release jobs. A real release uploads the three native
binaries and `skills.zip`, then creates the tag and GitLab Release. The dry run checks those
same four artifacts and the changelog without publishing.

## Optional custom images

`Dockerfile` pins Rust 1.96.0 and Python 3.14. The Rust version must match
`t32mcp-rs/rust-toolchain.toml`. The `rust-test` target contains clippy, rustfmt, nextest,
clang/libclang, LLVM/lld, and the simulator. `rust` adds Linux ARM64 gcc and cargo-xwin for
Windows. `pytest` supplies Python 3.14, uv, and the simulator.

The `trace32` build context must name an official TRACE32 installation. The image copies only
`bin/pc_linux64/t32mriscv`, `help.t32`, `per_notify.per`, and `t32pro.ps` into `/opt/t32`.
The simulator release is recorded in the `com.lauterbach.trace32.release` label. The build checks
that its shared libraries are present.

Choose a registry and **new, unpublished** tags before building. For example, with `REG`,
`RUST_TEST_TAG`, `RUST_TAG`, and `PYTEST_TAG` set to your chosen destinations:

```sh
podman build --build-context trace32=/path/to/T32 --target rust-test -t "$REG/rust-test:$RUST_TEST_TAG" -f ci/Dockerfile ci
podman build --build-context trace32=/path/to/T32 --target rust -t "$REG/rust:$RUST_TAG" -f ci/Dockerfile ci
podman build --build-context trace32=/path/to/T32 --target pytest -t "$REG/pytest:$PYTEST_TAG" -f ci/Dockerfile ci
```

Test and push each image before referencing it in CI. Never overwrite an existing tag; bump its
revision. Upstream Rust tags currently remain the defaults, with the dependency installation
described above. The default Python job continues using the public Python 3.14 image until an
explicit override selects a published custom image.

## Local verification and hooks

Run hardware-free helper checks from the locked tests environment:

```sh
uv run --no-env-file --project tests --locked --offline python -m unittest discover -s ci -p 'test_*.py'
bash -n ci/start_sim.sh ci/test_rust.sh
```

These helper tests use fake cargo, fake simulators, and a replaced socket boundary; they never
invoke a real build or debugger. The structural checks load both documents in `.gitlab-ci.yml`
and check image versions, release targets, and explicit opt-in settings.

The pre-commit hooks run gitleaks, rustfmt, locked clippy/build, `test_rust.sh`, and locked pytest
when Rust files change. Default hook runs stay offline. Explicit simulator runs require both
gates above. Packaging verification strips all `T32MCP*` variables, `T32_PORT`, `T32SYS`, and
uv env-file overrides, then forces `UV_NO_ENV_FILE=1`; its mocked tests check that isolation.
