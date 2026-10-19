// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use super::{OPERATION_LOCK, T32};
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
};
use t32mcp::{
    artifacts, catalog, diagnostics, image_verify, inspection, peripheral_views, peripherals,
    snapshot,
};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiffSnapshotsRequest {
    /// ID returned by capture_snapshot with save_artifact=true.
    pub before_id: String,
    /// ID of another saved snapshot in the configured artifact directory.
    pub after_id: String,
}

impl T32 {
    fn evidence_store(&self) -> artifacts::ArtifactStore {
        artifacts::ArtifactStore::new(self.options.artifacts_base.clone())
    }

    fn attach_evidence(&self, result: &mut serde_json::Value, save: bool, kind: &str) {
        result["artifact"] = if save {
            self.evidence_store()
                .save(kind, result)
                .unwrap_or_else(|error| {
                    // Persistence failure must not discard an already collected result.
                    serde_json::json!({"saved": false, "error": error})
                })
        } else {
            serde_json::json!({"saved": false, "reason": "not_requested"})
        };
    }
}

#[tool_router(router = native_router, vis = "pub(super)")]
impl T32 {
    #[tool(
        name = "doctor",
        description = "Diagnose the configured local TRACE32 endpoint with one lazy connection attempt, independent status queries and optional PER.FILENAME() metadata detection. --target-config declarations take precedence; disabled PER is not probed. A filename does not prove PER read support. Returns structured stages, native errors, capability provenance and hints even when PowerView is absent. Never retries, halts, resets, scans ports, loads PER files or probes target memory. Native calls still have no hard timeout.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn doctor(&self) -> Result<CallToolResult, String> {
        let _operation = OPERATION_LOCK.lock().await;
        let connection = self.ensure_inspection_connection_once();
        Ok(CallToolResult::structured(diagnostics::doctor(
            self.options.port,
            connection,
            self.options.target_config.per_support,
        )))
    }

    #[tool(
        name = "read_per_registers",
        description = "Read up to 64 exact, case-sensitive PER register/bitfield paths in ONE request across peripheral groups. PER support is chip/session-specific: an advertised tool does not establish support or a usable definition. Returns independent native address/value/optional BITFLD choice-string results and errors, without guessing raw addresses. Uses the current default PER definition only; no file loading, condition initialization or Watch-list edits. acknowledge_side_effects=true is REQUIRED: PER definitions can run dynamic logic including target writes, and MMIO reads can have side effects. Target must ALREADY be halted/stopped; never auto-halts or retries. Sequential, not atomic; soft deadline cannot interrupt native calls."
    )]
    async fn read_per_registers(
        &self,
        Parameters(request): Parameters<peripherals::ReadPerRegistersRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        if let Err(error) = self.options.target_config.per_support.require_per() {
            return Ok(CallToolResult::structured(
                peripherals::configured_unavailable(&request, &error)?,
            ));
        }
        let _operation = OPERATION_LOCK.lock().await;
        let result = match self.ensure_inspection_connection_once() {
            Ok(()) => peripherals::read_registers(&request)?,
            Err(error) => peripherals::connection_failure(&request, &error)?,
        };
        Ok(CallToolResult::structured(result))
    }

    #[tool(
        name = "read_per_views",
        description = "Browse up to 16 selected PER subtrees in ONE request, returning separate bounded text pages and continuation offsets. PER support is chip/session-specific: an advertised tool does not establish support or a usable definition. Supply subtree as literal tree-title components; expand=true expands only that selected subtree. Empty subtree with expand=false shows the default PER view; root expand-all is refused. Uses the current default PER definition, never loads a file or changes PER.Watch. acknowledge_side_effects=true REQUIRED because PER interpretation/MMIO may have side effects, including writes in trusted definitions. Target must ALREADY be halted/stopped. No automatic halt, retry or condition refresh; not an atomic snapshot."
    )]
    async fn read_per_views(
        &self,
        Parameters(request): Parameters<peripheral_views::ReadPerViewsRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        if let Err(error) = self.options.target_config.per_support.require_per() {
            return Ok(CallToolResult::structured(
                peripheral_views::configured_unavailable(&request, &error),
            ));
        }
        let _operation = OPERATION_LOCK.lock().await;
        let result = match self.ensure_inspection_connection_once() {
            Ok(()) => peripheral_views::read_views(&request)?,
            Err(error) => peripheral_views::connection_failure(&request, &error),
        };
        Ok(CallToolResult::structured(result))
    }

    #[tool(
        name = "capture_snapshot",
        description = "Collect bounded, uniquely named status/expression/register/memory/symbol observations with independent expectations, errors and timestamps. Reads are sequential, NOT an atomic snapshot. No automatic run control; expressions and MMIO are not sandboxed. save_artifact=true explicitly persists the full snapshot (possibly sensitive bytes) locally for diff_snapshots; defaults to false."
    )]
    async fn capture_snapshot(
        &self,
        Parameters(request): Parameters<snapshot::CaptureSnapshotRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        let save = request.save_artifact;
        let _operation = OPERATION_LOCK.lock().await;
        if save {
            self.evidence_store().prepare()?;
        }
        let mut result = match self.ensure_inspection_connection_once() {
            Ok(()) => snapshot::capture(request)?,
            Err(error) => snapshot::capture_connection_failure(request, error)?,
        };
        self.attach_evidence(&mut result, save, "snapshot");
        Ok(CallToolResult::structured(result))
    }

    #[tool(
        name = "diff_snapshots",
        description = "Compare two locally saved snapshots by their 32-hex-character artifact IDs. Matches named items only when query semantics agree, ignoring collection timestamps. Reports changed, unchanged, added, removed, incompatible and unavailable items; failed reads are never equal evidence. Offline: no debugger connection or target access.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn diff_snapshots(
        &self,
        Parameters(request): Parameters<DiffSnapshotsRequest>,
    ) -> Result<CallToolResult, String> {
        artifacts::validate_id(&request.before_id)?;
        artifacts::validate_id(&request.after_id)?;
        let store = self.evidence_store();
        let before = store.load(&request.before_id, "snapshot")?;
        let after = store.load(&request.after_id, "snapshot")?;
        let mut result = snapshot::diff_snapshots(&before, &after)?;
        result["before_id"] = serde_json::json!(request.before_id);
        result["after_id"] = serde_json::json!(request.after_id);
        Ok(CallToolResult::structured(result))
    }

    #[tool(
        name = "verify_image",
        description = "Compare a bounded local BIN or Intel HEX image with explicitly allowed 32-bit target address ranges. BIN requires base_address; HEX holes are NOT filled. Validates the entire file before connecting. Target must ALREADY be halted/stopped and is checked between reads; never halts, flashes or retries. Returns hashes, bounded mismatch addresses and precise partial coverage, not raw memory or signature authentication. Deadline is checked between native calls only. Optional save_artifact persists the report locally."
    )]
    async fn verify_image(
        &self,
        Parameters(request): Parameters<image_verify::VerifyImageRequest>,
    ) -> Result<CallToolResult, String> {
        let save = request.save_artifact;
        let prepared = image_verify::prepare(request)?;
        let _operation = OPERATION_LOCK.lock().await;
        if save {
            self.evidence_store().prepare()?;
        }
        let mut result = match self.ensure_inspection_connection_once() {
            Ok(()) => image_verify::verify(prepared),
            Err(error) => image_verify::verify_with(
                prepared,
                || Err(error.clone()),
                |_, _, _, _| Err("No connection; memory was not read".into()),
            ),
        };
        self.attach_evidence(&mut result, save, "image_verification");
        Ok(CallToolResult::structured(result))
    }

    #[tool(
        name = "get_debugger_status",
        description = "Read the configured local TRACE32 connection, target state (0=down, 1=halted, 2=stopped, 3=running), PRACTICE state, CPU and software version. Does not halt, run, reset or load a target. Optional fields retain individual errors.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn get_debugger_status(&self) -> Result<CallToolResult, String> {
        let _operation = OPERATION_LOCK.lock().await;
        self.establish_debug_connection(true)?;
        Ok(CallToolResult::structured(inspection::status(
            self.options.port,
        )))
    }

    #[tool(
        name = "evaluate_expressions",
        description = "Evaluate up to 64 TRACE32 expressions in order, returning native value text, result type and per-item errors. Decimal results may have a trailing dot. This is a query interface, not a sandbox: only use observation expressions; target-specific functions and MMIO reads can have side effects."
    )]
    async fn evaluate_expressions(
        &self,
        Parameters(request): Parameters<inspection::EvaluateRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        let _operation = OPERATION_LOCK.lock().await;
        self.establish_debug_connection(true)?;
        inspection::evaluate(request).map(CallToolResult::structured)
    }

    #[tool(
        name = "read_registers",
        description = "Read up to 64 named CPU registers using the native Remote API. Returns full 64-bit hexadecimal values and individual errors without halting or resuming the target.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn read_registers(
        &self,
        Parameters(request): Parameters<inspection::RegistersRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        let _operation = OPERATION_LOCK.lock().await;
        self.establish_debug_connection(true)?;
        inspection::registers(request.names).map(CallToolResult::structured)
    }

    #[tool(
        name = "read_memory",
        description = "Read bounded 32-bit-address memory regions (1..4096 bytes each, 65536 total) as hex and hexdump. Optionally compare exact expected_hex bytes and report mismatches. Select data, program or arm_dap and optional dual_port. Access-mode support depends on the target and session; ARM DAP is not a portable fallback. This server only permits running-target requests with arm_dap and dual_port=true (explicit EDAP); generic data/program or E-access is refused because it can use StopAndGo. Does not issue explicit write or run-control commands. MMIO reads can acknowledge/clear peripheral state."
    )]
    async fn read_memory(
        &self,
        Parameters(request): Parameters<inspection::MemoryRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        let _operation = OPERATION_LOCK.lock().await;
        self.establish_debug_connection(true)?;
        inspection::memory(request).map(CallToolResult::structured)
    }

    #[tool(
        name = "lookup_symbols",
        description = "Look up up to 64 exact loaded symbols, returning 32-bit address, size and raw legacy access/reserved metadata. Does not load an ELF or modify symbols. Missing symbols retain individual errors.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn lookup_symbols(
        &self,
        Parameters(request): Parameters<inspection::SymbolsRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        let _operation = OPERATION_LOCK.lock().await;
        self.establish_debug_connection(true)?;
        inspection::symbols(request.names).map(CallToolResult::structured)
    }

    #[tool(
        name = "read_debugger_window",
        description = "Read a bounded page of register, frame, var_local, break_list or task_list window text through the native API. Use next_offset for pagination. Window support depends on CPU, debug symbols and OS awareness. Does not execute arbitrary commands or use printer files."
    )]
    async fn read_debugger_window(
        &self,
        Parameters(request): Parameters<inspection::WindowRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        let _operation = OPERATION_LOCK.lock().await;
        self.establish_debug_connection(true)?;
        inspection::window(request).map(CallToolResult::structured)
    }

    #[tool(
        name = "wait_for_condition",
        description = "Poll an observation expression until its exact native text equals expected_value or a bounded deadline expires. Retains timestamped samples, errors and attempts (not just final success). Does not issue explicit run-control commands; expression evaluation is not a side-effect sandbox. Deadline is checked BETWEEN native calls; it cannot interrupt a blocked vendor API call. All calls to this server are serialized while waiting."
    )]
    async fn wait_for_condition(
        &self,
        Parameters(request): Parameters<inspection::WaitRequest>,
    ) -> Result<CallToolResult, String> {
        request.validate()?;
        let _operation = OPERATION_LOCK.lock().await;
        self.establish_debug_connection(true)?;
        inspection::wait(request)
            .await
            .map(CallToolResult::structured)
    }

    #[tool(
        name = "list_practice_skills",
        description = "List installed skill names, script filenames and SKILL.md paths in the configured --skills directory. Does not read or run PRACTICE scripts and requires no TRACE32 connection.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn list_practice_skills(&self) -> Result<CallToolResult, String> {
        catalog::list_skills(std::path::Path::new(&self.options.skills_base))
            .map(CallToolResult::structured)
    }
}
