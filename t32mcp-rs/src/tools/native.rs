// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Opt-in local observations on the same worker as upstream script execution.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{Context, ToolEntry, ToolHandler, call_result, error_result, worker_call};
use crate::{
    artifacts, capabilities::TargetConfig, catalog, diagnostics, image_verify, inspection, mcp,
    peripheral_views, peripherals, snapshot, t32,
};

#[derive(Debug, Clone)]
pub struct NativeOptions {
    pub target_config: TargetConfig,
    pub artifacts_base: PathBuf,
}

impl Default for NativeOptions {
    fn default() -> Self {
        let state_home = if cfg!(target_os = "windows") {
            std::env::var_os("LOCALAPPDATA")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        } else {
            std::env::var_os("XDG_STATE_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|value| !value.is_empty())
                        .map(|home| PathBuf::from(home).join(".local").join("state"))
                })
        };
        Self {
            target_config: TargetConfig::default(),
            artifacts_base: state_home
                .map(|root| root.join("t32mcp").join("evidence"))
                .unwrap_or_default(),
        }
    }
}

impl NativeOptions {
    fn store(&self) -> artifacts::ArtifactStore {
        artifacts::ArtifactStore::new(self.artifacts_base.clone())
    }

    fn attach_evidence(&self, result: &mut Value, save: bool, kind: &str) {
        result["artifact"] = if save {
            self.store()
                .save(kind, result)
                .unwrap_or_else(|error| json!({"saved": false, "error": error}))
        } else {
            json!({"saved": false, "reason": "not_requested"})
        };
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyRequest {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiffSnapshotsRequest {
    /// ID returned by capture_snapshot with save_artifact=true.
    before_id: String,
    /// ID of another saved snapshot in the configured artifact directory.
    after_id: String,
}

fn options(ctx: &Context) -> Result<&NativeOptions, String> {
    ctx.native
        .as_ref()
        .ok_or_else(|| "This tool requires --native-tools".to_string())
}

fn parse<T: DeserializeOwned>(
    args: Option<&Value>,
    hardware: bool,
) -> Result<(Option<u16>, T), String> {
    let mut arguments = match args {
        None => serde_json::Map::new(),
        Some(Value::Object(object)) => object.clone(),
        Some(_) => return Err("Tool arguments must be a JSON object".to_string()),
    };
    let port = if hardware {
        match arguments.remove("port") {
            None | Some(Value::Null) => None,
            Some(value) => match value.as_u64().and_then(|port| u16::try_from(port).ok()) {
                Some(port) if port > 0 => Some(port),
                _ => return Err("port must be an integer between 1 and 65535".to_string()),
            },
        }
    } else {
        None
    };
    serde_json::from_value(Value::Object(arguments))
        .map(|request| (port, request))
        .map_err(|error| format!("Invalid native tool arguments: {error}"))
}

fn result(value: Result<Value, String>) -> mcp::CallToolResult {
    match value {
        Ok(value) => call_result(value.to_string(), None, Some(value)),
        Err(error) => error_result(&error),
    }
}

struct ConnectionCleanup;

impl Drop for ConnectionCleanup {
    fn drop(&mut self) {
        crate::t32_disconnect();
    }
}

fn hardware(
    ctx: &Context,
    port: Option<u16>,
    operation: impl FnOnce(u16, Result<(), String>) -> Result<Value, String> + Send + 'static,
) -> Result<Value, String> {
    options(ctx)?;
    worker_call(ctx, ctx.request_timeout, |response_tx| {
        t32::WorkerRequest::Native {
            port,
            operation: Box::new(move |port| {
                // The channel is created, used and freed on this one worker thread.
                // A failed observation is never retried on this or another instance.
                let _cleanup = ConnectionCleanup;
                let connection = crate::t32_connect(port);
                let mut value = operation(port, connection)?;
                value["instance_port"] = json!(port);
                Ok(value)
            }),
            response_tx,
        }
    })?
}

fn doctor(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
    result((|| {
        let options = options(ctx)?.clone();
        let (port, _) = parse::<EmptyRequest>(args, true)?;
        hardware(ctx, port, move |port, connection| {
            Ok(diagnostics::doctor(
                port,
                connection,
                options.target_config.per_support,
            ))
        })
    })())
}

fn read_per_registers(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
    result((|| {
        let options = options(ctx)?;
        let (port, request) = parse::<peripherals::ReadPerRegistersRequest>(args, true)?;
        request.validate()?;
        if let Err(error) = options.target_config.per_support.require_per() {
            return peripherals::configured_unavailable(&request, &error);
        }
        hardware(ctx, port, move |_, connection| match connection {
            Ok(()) => peripherals::read_registers(&request),
            Err(error) => peripherals::connection_failure(&request, &error),
        })
    })())
}

fn read_per_views(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
    result((|| {
        let options = options(ctx)?;
        let (port, request) = parse::<peripheral_views::ReadPerViewsRequest>(args, true)?;
        request.validate()?;
        if let Err(error) = options.target_config.per_support.require_per() {
            return Ok(peripheral_views::configured_unavailable(&request, &error));
        }
        hardware(ctx, port, move |_, connection| match connection {
            Ok(()) => peripheral_views::read_views(&request),
            Err(error) => Ok(peripheral_views::connection_failure(&request, &error)),
        })
    })())
}

fn capture_snapshot(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
    result((|| {
        let options = options(ctx)?.clone();
        let (port, request) = parse::<snapshot::CaptureSnapshotRequest>(args, true)?;
        request.validate()?;
        if request.save_artifact {
            options.store().prepare()?;
        }
        hardware(ctx, port, move |port, connection| {
            capture_snapshot_result(&options, request, port, connection)
        })
    })())
}

fn capture_snapshot_result(
    options: &NativeOptions,
    request: snapshot::CaptureSnapshotRequest,
    port: u16,
    connection: Result<(), String>,
) -> Result<Value, String> {
    let save = request.save_artifact;
    let mut value = match connection {
        Ok(()) => snapshot::capture(request)?,
        Err(error) => snapshot::capture_connection_failure(request, error)?,
    };
    value["instance_port"] = json!(port);
    options.attach_evidence(&mut value, save, "snapshot");
    Ok(value)
}

fn diff_snapshots(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
    result((|| {
        let options = options(ctx)?;
        let (_, request) = parse::<DiffSnapshotsRequest>(args, false)?;
        artifacts::validate_id(&request.before_id)?;
        artifacts::validate_id(&request.after_id)?;
        let store = options.store();
        let before = store.load(&request.before_id, "snapshot")?;
        let after = store.load(&request.after_id, "snapshot")?;
        let instance_comparison = snapshot_instance_comparison(&before, &after)?;
        let mut value = snapshot::diff_snapshots(&before, &after)?;
        value["before_id"] = json!(request.before_id);
        value["after_id"] = json!(request.after_id);
        value["instance_comparison"] = instance_comparison;
        Ok(value)
    })())
}

fn snapshot_instance_comparison(before: &Value, after: &Value) -> Result<Value, String> {
    let port = |snapshot: &Value| match snapshot.get("instance_port") {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port > 0)
            .map(Some)
            .ok_or_else(|| "Snapshot has an invalid instance_port".to_string()),
    };
    match (port(before)?, port(after)?) {
        (Some(before), Some(after)) if before != after => Err(format!(
            "Cannot compare snapshots from different PowerView instance ports: {before} and {after}"
        )),
        (Some(port), Some(_)) => Ok(json!({
            "status": "matching_recorded_port",
            "port": port,
            "note": "Matching ports do not establish unchanged target configuration or core selection."
        })),
        _ => Ok(json!({
            "status": "unknown",
            "reason": "At least one legacy snapshot has no instance_port; comparison proceeds without verified instance identity."
        })),
    }
}

fn verify_image(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
    result((|| {
        let options = options(ctx)?.clone();
        let (port, request) = parse::<image_verify::VerifyImageRequest>(args, true)?;
        let save = request.save_artifact;
        // Validate all image bytes and permitted ranges before contacting hardware.
        let prepared = image_verify::prepare(request)?;
        if save {
            options.store().prepare()?;
        }
        hardware(ctx, port, move |port, connection| {
            let mut value = match connection {
                Ok(()) => image_verify::verify(prepared),
                Err(error) => image_verify::verify_with(
                    prepared,
                    || Err(error.clone()),
                    |_, _, _, _| Err("No connection; memory was not read".into()),
                ),
            };
            value["instance_port"] = json!(port);
            options.attach_evidence(&mut value, save, "image_verification");
            Ok(value)
        })
    })())
}

fn get_debugger_status(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
    result((|| {
        options(ctx)?;
        let (port, _) = parse::<EmptyRequest>(args, true)?;
        hardware(ctx, port, |port, connection| {
            connection?;
            Ok(inspection::status(port))
        })
    })())
}

macro_rules! inspection_handler {
    ($name:ident, $request:ty, $operation:expr) => {
        fn $name(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
            result((|| {
                options(ctx)?;
                let (port, request) = parse::<$request>(args, true)?;
                request.validate()?;
                hardware(ctx, port, move |_, connection| {
                    connection?;
                    ($operation)(request)
                })
            })())
        }
    };
}

inspection_handler!(
    evaluate_expressions,
    inspection::EvaluateRequest,
    inspection::evaluate
);
inspection_handler!(
    read_registers,
    inspection::RegistersRequest,
    |request: inspection::RegistersRequest| inspection::registers(request.names)
);
inspection_handler!(read_memory, inspection::MemoryRequest, inspection::memory);
inspection_handler!(
    lookup_symbols,
    inspection::SymbolsRequest,
    |request: inspection::SymbolsRequest| inspection::symbols(request.names)
);
inspection_handler!(
    read_debugger_window,
    inspection::WindowRequest,
    inspection::window
);
inspection_handler!(
    wait_for_condition,
    inspection::WaitRequest,
    |request: inspection::WaitRequest| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|error| format!("Cannot start native wait timer: {error}"))?
            .block_on(inspection::wait(request))
    }
);

fn list_practice_skills(args: Option<&Value>, ctx: &mut Context) -> mcp::CallToolResult {
    result((|| {
        options(ctx)?;
        parse::<EmptyRequest>(args, false)?;
        if ctx.skill_roots.is_empty() {
            return Err("No --skills directories configured; use --skills <directory>".to_string());
        }
        if ctx.skill_roots.len() == 1 {
            return catalog::list_skills(&ctx.skill_roots[0]);
        }
        let mut skills = Vec::new();
        let mut roots = Vec::new();
        let mut truncated = false;
        for root in &ctx.skill_roots {
            let value = catalog::list_skills(root)?;
            truncated |= value["truncated"].as_bool().unwrap_or(false);
            roots.push(value["skills_base"].clone());
            for mut skill in value["skills"].as_array().cloned().unwrap_or_default() {
                skill["skills_base"] = value["skills_base"].clone();
                skills.push(skill);
            }
        }
        Ok(json!({"skills_roots": roots, "skills": skills, "truncated": truncated}))
    })())
}

fn entry<T: JsonSchema>(
    name: &str,
    description: &str,
    handler: ToolHandler,
    read_only_hint: bool,
    hardware: bool,
) -> ToolEntry {
    let schema = schemars::schema_for!(T);
    let mut schema = serde_json::to_value(schema)
        .expect("request schema is serializable")
        .as_object()
        .expect("request schema is an object")
        .clone();
    let dialect = schema
        .remove("$schema")
        .and_then(|value| value.as_str().map(str::to_string));
    schema.remove("type");
    if hardware {
        schema
            .entry("properties".to_string())
            .or_insert_with(|| json!({}))["port"] = json!({
            "type": ["integer", "null"],
            "minimum": 1,
            "maximum": 65535,
            "description": "Registered local PowerView port. Required if multiple instances are registered. No automatic fallback; uses the currently selected core."
        });
    }
    ToolEntry {
        tool: mcp::Tool {
            annotations: read_only_hint.then_some(mcp::ToolAnnotations {
                read_only_hint: Some(true),
                destructive_hint: Some(false),
                idempotent_hint: None,
                open_world_hint: None,
                title: None,
            }),
            description: Some(if hardware {
                format!(
                    "{description} Optional port selects a registered instance; required for \
                     multiple instances. Uses the current core, without selecting another one."
                )
            } else {
                description.to_string()
            }),
            icons: Vec::new(),
            input_schema: mcp::ToolInputSchema {
                type_: "object".to_string(),
                extra: schema,
                schema: dialect,
            },
            meta: None,
            name: name.to_string(),
            output_schema: Some(mcp::ToolOutputSchema {
                schema: None,
                extra: json!({"type": "object"}).as_object().unwrap().clone(),
            }),
            title: None,
        },
        handler,
    }
}

pub fn entries() -> Vec<ToolEntry> {
    vec![
        entry::<EmptyRequest>(
            "doctor",
            "Diagnose the configured local TRACE32 endpoint with one lazy connection attempt, independent status queries and optional PER.FILENAME() metadata detection. --target-config declarations take precedence; disabled PER is not probed. A filename does not prove PER read support. Returns structured stages, native errors, capability provenance and hints even when PowerView is absent. Never retries, halts, resets, scans ports, loads PER files or probes target memory. The server request deadline limits caller waiting but cannot interrupt a blocked vendor API call.",
            doctor,
            true,
            true,
        ),
        entry::<peripherals::ReadPerRegistersRequest>(
            "read_per_registers",
            "Read up to 64 exact, case-sensitive PER register/bitfield paths in ONE request across peripheral groups. PER support is chip/session-specific: an advertised tool does not establish support or a usable definition. Returns independent native address/value/optional BITFLD choice-string results and errors, without guessing raw addresses. Uses the current default PER definition only; no file loading, condition initialization or Watch-list edits. acknowledge_side_effects=true is REQUIRED: PER definitions can run dynamic logic including target writes, and MMIO reads can have side effects. Target must ALREADY be halted/stopped; never auto-halts or retries. Sequential, not atomic; soft deadline cannot interrupt native calls.",
            read_per_registers,
            false,
            true,
        ),
        entry::<peripheral_views::ReadPerViewsRequest>(
            "read_per_views",
            "Browse up to 16 selected PER subtrees in ONE request, returning separate bounded text pages and continuation offsets. PER support is chip/session-specific: an advertised tool does not establish support or a usable definition. Supply subtree as literal tree-title components; expand=true expands only that selected subtree. Empty subtree with expand=false shows the default PER view; root expand-all is refused. Uses the current default PER definition, never loads a file or changes PER.Watch. acknowledge_side_effects=true REQUIRED because PER interpretation/MMIO may have side effects, including writes in trusted definitions. Target must ALREADY be halted/stopped. No automatic halt, retry or condition refresh; not an atomic snapshot.",
            read_per_views,
            false,
            true,
        ),
        entry::<snapshot::CaptureSnapshotRequest>(
            "capture_snapshot",
            "Collect bounded, uniquely named status/expression/register/memory/symbol observations with independent expectations, errors and timestamps. Reads are sequential, NOT an atomic snapshot. No automatic run control; expressions and MMIO are not sandboxed. save_artifact=true explicitly persists the full snapshot (possibly sensitive bytes) locally for diff_snapshots; defaults to false.",
            capture_snapshot,
            false,
            true,
        ),
        entry::<DiffSnapshotsRequest>(
            "diff_snapshots",
            "Compare two locally saved snapshots by their 32-hex-character artifact IDs. Matches named items only when query semantics agree, ignoring collection timestamps. Reports changed, unchanged, added, removed, incompatible and unavailable items; failed reads are never equal evidence. Different recorded instance ports are refused. Legacy snapshots without instance_port remain comparable, with explicitly unknown instance identity. Offline: no debugger connection or target access.",
            diff_snapshots,
            true,
            false,
        ),
        entry::<image_verify::VerifyImageRequest>(
            "verify_image",
            "Compare a bounded local BIN or Intel HEX image with explicitly allowed 32-bit target address ranges. BIN requires base_address; HEX holes are NOT filled. Validates the entire file before connecting. Target must ALREADY be halted/stopped and is checked between reads; never halts, flashes or retries. Returns hashes, bounded mismatch addresses and precise partial coverage, not raw memory or signature authentication. Deadline is checked between native calls only. Optional save_artifact persists the report locally.",
            verify_image,
            false,
            true,
        ),
        entry::<EmptyRequest>(
            "get_debugger_status",
            "Read the configured local TRACE32 connection, target state (0=down, 1=halted, 2=stopped, 3=running), PRACTICE state, CPU and software version. Does not halt, run, reset or load a target. Optional fields retain individual errors.",
            get_debugger_status,
            true,
            true,
        ),
        entry::<inspection::EvaluateRequest>(
            "evaluate_expressions",
            "Evaluate up to 64 TRACE32 expressions in order, returning native value text, result type and per-item errors. Decimal results may have a trailing dot. This is a query interface, not a sandbox: only use observation expressions; target-specific functions and MMIO reads can have side effects.",
            evaluate_expressions,
            false,
            true,
        ),
        entry::<inspection::RegistersRequest>(
            "read_registers",
            "Read up to 64 named CPU registers using the native Remote API. Returns full 64-bit hexadecimal values and individual errors without halting or resuming the target.",
            read_registers,
            true,
            true,
        ),
        entry::<inspection::MemoryRequest>(
            "read_memory",
            "Read bounded 32-bit-address memory regions (1..4096 bytes each, 65536 total) as hex and hexdump. Optionally compare exact expected_hex bytes and report mismatches. Select data, program or arm_dap and optional dual_port. Access-mode support depends on the target and session; ARM DAP is not a portable fallback. This server only permits running-target requests with arm_dap and dual_port=true (explicit EDAP); generic data/program or E-access is refused because it can use StopAndGo. Does not issue explicit write or run-control commands. MMIO reads can acknowledge/clear peripheral state.",
            read_memory,
            false,
            true,
        ),
        entry::<inspection::SymbolsRequest>(
            "lookup_symbols",
            "Look up up to 64 exact loaded symbols, returning 32-bit address, size and raw legacy access/reserved metadata. Does not load an ELF or modify symbols. Missing symbols retain individual errors.",
            lookup_symbols,
            true,
            true,
        ),
        entry::<inspection::WindowRequest>(
            "read_debugger_window",
            "Read a bounded page of register, frame, var_local, break_list or task_list window text through the native API. Use next_offset for pagination. Window support depends on CPU, debug symbols and OS awareness. Does not execute arbitrary commands or use printer files.",
            read_debugger_window,
            false,
            true,
        ),
        entry::<inspection::WaitRequest>(
            "wait_for_condition",
            "Poll an observation expression until its exact native text equals expected_value or a bounded deadline expires. Retains timestamped samples, errors and attempts (not just final success). Does not issue explicit run-control commands; expression evaluation is not a side-effect sandbox. Deadline is checked BETWEEN native calls; it cannot interrupt a blocked vendor API call. All calls to this server are serialized while waiting.",
            wait_for_condition,
            false,
            true,
        ),
        entry::<EmptyRequest>(
            "list_practice_skills",
            "List installed skill names, script filenames and SKILL.md paths in the configured --skills directories. Does not read or run PRACTICE scripts and requires no TRACE32 connection.",
            list_practice_skills,
            true,
            false,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::mpsc,
        time::{Duration, Instant},
    };

    fn context(native: bool) -> (Context, mpsc::Receiver<t32::QueuedRequest>) {
        let (worker_tx, worker_rx) = mpsc::sync_channel(t32::WORKER_QUEUE_CAPACITY);
        (
            Context {
                event_queue: VecDeque::new(),
                instructions: String::new(),
                policy: crate::execution_policy::ExecutionPolicy::Unrestricted,
                skill_roots: Vec::new(),
                read_only: false,
                native: native.then(NativeOptions::default),
                request_started: Instant::now(),
                request_timeout: Duration::from_secs(60),
                tools: Vec::new(),
                worker_tx,
            },
            worker_rx,
        )
    }

    #[test]
    fn exposes_exactly_fourteen_unique_bounded_schemas() {
        let tools = entries();
        let names: std::collections::HashSet<_> =
            tools.iter().map(|entry| &entry.tool.name).collect();
        assert_eq!(tools.len(), 14);
        assert_eq!(names.len(), 14);
        for entry in tools {
            let schema = serde_json::to_value(&entry.tool.input_schema).unwrap();
            let offline = matches!(
                entry.tool.name.as_str(),
                "list_practice_skills" | "diff_snapshots"
            );
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
            assert_eq!(schema["properties"].get("port").is_none(), offline);
            assert!(
                !schema["required"].as_array().is_some_and(|items| {
                    items.iter().any(|item| item.as_str() == Some("port"))
                })
            );
            assert_eq!(entry.tool.output_schema.unwrap().extra["type"], "object");
        }
    }

    #[test]
    fn port_parsing_preserves_strict_request_validation() {
        for port in [
            json!(0),
            json!(65536),
            json!(-1),
            json!(1.5),
            json!("20000"),
        ] {
            assert!(parse::<EmptyRequest>(Some(&json!({"port": port})), true).is_err());
        }
        assert!(parse::<EmptyRequest>(Some(&json!([])), true).is_err());
        assert!(parse::<EmptyRequest>(Some(&json!({"core": 1})), true).is_err());
        assert!(parse::<EmptyRequest>(Some(&json!({"port": 20000})), false).is_err());
        assert_eq!(
            parse::<EmptyRequest>(Some(&json!({"port": 20000})), true)
                .unwrap()
                .0,
            Some(20000)
        );
        assert_eq!(
            parse::<EmptyRequest>(Some(&json!({"port": null})), true)
                .unwrap()
                .0,
            None
        );
        assert!(
            parse::<inspection::RegistersRequest>(
                Some(&json!({"port": 20000, "names": ["PC"], "ignored": true})),
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn every_native_handler_requires_explicit_opt_in() {
        let (mut ctx, rx) = context(false);
        for entry in entries() {
            let value = (entry.handler)(None, &mut ctx);
            assert_eq!(value.is_error, Some(true), "{}", entry.tool.name);
            assert!(
                serde_json::to_string(&value)
                    .unwrap()
                    .contains("--native-tools"),
                "{}",
                entry.tool.name
            );
        }
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn invalid_native_requests_do_not_reach_the_worker() {
        let (mut ctx, rx) = context(true);
        for (handler, args) in [
            (doctor as ToolHandler, json!({"port": 0})),
            (evaluate_expressions, json!({"expressions": []})),
            (read_registers, json!({"names": ["PC"], "core": 1})),
            (read_memory, json!({"regions": []})),
            (wait_for_condition, json!({"expression": "STATE.RUN()"})),
            (list_practice_skills, json!({"port": 20000})),
        ] {
            assert_eq!(handler(Some(&args), &mut ctx).is_error, Some(true));
        }
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn read_only_and_execution_policy_are_enforced_by_handlers() {
        use crate::tools::{trace32_manage_instances, trace32_run_command, trace32_run_script};

        let (mut ctx, rx) = context(true);
        ctx.read_only = true;
        for handler in [
            trace32_run_command::trace32_run_command as ToolHandler,
            trace32_run_script::trace32_run_script,
            trace32_manage_instances::trace32_manage_instances,
        ] {
            let value = handler(None, &mut ctx);
            assert_eq!(value.is_error, Some(true));
            assert!(
                serde_json::to_string(&value)
                    .unwrap()
                    .contains("--read-only")
            );
        }
        ctx.read_only = false;
        ctx.policy = crate::execution_policy::ExecutionPolicy::Strict;
        for handler in [
            trace32_run_command::trace32_run_command as ToolHandler,
            trace32_manage_instances::trace32_manage_instances,
        ] {
            let value = handler(None, &mut ctx);
            assert_eq!(value.is_error, Some(true));
            assert!(
                serde_json::to_string(&value)
                    .unwrap()
                    .contains("--execution-policy")
            );
        }
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn snapshot_diff_rejects_cross_instance_evidence_without_breaking_legacy_files() {
        let first = json!({"instance_port": 20000});
        let second = json!({"instance_port": 20001});
        assert!(
            snapshot_instance_comparison(&first, &second)
                .unwrap_err()
                .contains("different PowerView instance ports")
        );
        assert_eq!(
            snapshot_instance_comparison(&first, &first).unwrap()["status"],
            "matching_recorded_port"
        );
        for legacy in [json!({}), json!({"items": []})] {
            assert_eq!(
                snapshot_instance_comparison(&first, &legacy).unwrap()["status"],
                "unknown"
            );
            assert_eq!(
                snapshot_instance_comparison(&legacy, &legacy).unwrap()["status"],
                "unknown"
            );
        }
        for invalid in [json!(0), json!(65536), json!("20000"), json!(null)] {
            assert!(
                snapshot_instance_comparison(&first, &json!({"instance_port": invalid})).is_err()
            );
        }
    }

    #[test]
    fn captured_snapshot_artifacts_with_instance_metadata_roundtrip_through_offline_diff() {
        let directory = tempfile::tempdir().unwrap();
        let (mut ctx, rx) = context(true);
        ctx.native.as_mut().unwrap().artifacts_base = directory.path().to_path_buf();
        let options = options(&ctx).unwrap();
        options.store().prepare().unwrap();
        let mut saved = Vec::new();
        for _ in 0..2 {
            let request = serde_json::from_value(json!({
                "items": [{"name": "state", "query": {"kind": "status"}}],
                "save_artifact": true
            }))
            .unwrap();
            let value = capture_snapshot_result(
                options,
                request,
                20000,
                Err("Test connection unavailable; no native API called".to_string()),
            )
            .unwrap();
            assert_eq!(value["artifact"]["saved"], true);
            assert_eq!(value["instance_port"], 20000);
            saved.push(value["artifact"]["id"].as_str().unwrap().to_string());
        }
        let payload = options.store().load(&saved[0], "snapshot").unwrap();
        assert_eq!(payload["instance_port"], 20000);
        let response = diff_snapshots(
            Some(&json!({"before_id": saved[0], "after_id": saved[1]})),
            &mut ctx,
        );
        assert_ne!(
            response.is_error,
            Some(true),
            "{}",
            serde_json::to_string(&response).unwrap()
        );
        let difference = response.structured_content.unwrap();
        assert_eq!(difference["format"], "t32mcp.snapshot_diff");
        assert_eq!(difference["counts"]["unavailable"], 1);
        assert_eq!(
            difference["instance_comparison"]["status"],
            "matching_recorded_port"
        );
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }
}
