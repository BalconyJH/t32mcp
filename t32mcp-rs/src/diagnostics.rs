// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Passive diagnostics over one caller-owned connection attempt or session.
//! Callers serialize access to the native API. No retries, memory probes, or
//! target-control operations are performed here; native calls can still block.

use crate::capabilities::{self, PerSupport};
use serde_json::{Map, Value, json};

const STATUS_FIELDS: [&str; 5] = [
    "state",
    "practice_state",
    "software_version",
    "build",
    "cpu",
];

fn error_value(code: &str, stage: &str, message: &str, hint: &str) -> Value {
    json!({
        "code": code,
        "stage": stage,
        "message": message,
        "actionable_hint": hint,
    })
}

fn native_error(message: &str) -> Option<(&str, i32)> {
    let (operation, suffix) = message.split_once(" failed (Remote API error ")?;
    if !operation.starts_with("T32_")
        || !operation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let code = suffix.strip_suffix(')')?.parse().ok()?;
    Some((operation, code))
}

/// Add stable categories without guessing a vendor error's underlying cause.
/// Unrecognized errors retain their original message and caller-provided stage.
pub fn classify_error(stage: &str, message: &str) -> Value {
    if message == "TRACE32 TCP port must be between 1 and 65535" {
        return error_value(
            "INVALID_CONFIGURATION",
            "configuration",
            message,
            "Configure a TCP port between 1 and 65535.",
        );
    }
    if message.starts_with("Cannot reach TRACE32 at 127.0.0.1:") {
        let mut error = error_value(
            "TCP_PREFLIGHT_FAILED",
            "tcp_preflight",
            message,
            "Check the configured local port and PowerView RCL=NETTCP listener. \
             Endpoint failure does not establish whether PowerView is absent; a powered \
             board alone does not provide a Remote API listener.",
        );
        error["operation"] = json!("tcp_connect");
        return error;
    }
    if let Some(code) =
        message.strip_prefix("Connection with TRACE32 failed: T32_RequestChannelNetTcp returned ")
    {
        let mut error = error_value(
            "CHANNEL_CREATE_FAILED",
            "channel",
            message,
            "Check the local Remote API runtime and retain this native error for diagnosis. \
             No target state can be inferred from channel allocation failure.",
        );
        error["operation"] = json!("T32_RequestChannelNetTcp");
        if let Ok(code) = code.parse::<i32>() {
            error["native_code"] = json!(code);
        }
        return error;
    }
    if message == "No TRACE32 connection on this thread; connect before querying the native API" {
        return error_value(
            "NOT_CONNECTED",
            stage,
            message,
            "Establish a Remote API connection on the thread performing the query. \
             This does not prove that PowerView or the board is unavailable.",
        );
    }
    if let Some((operation, native_code)) = native_error(message) {
        let (code, stage, hint) = match operation {
            "T32_Config" => (
                "API_CONFIGURATION_FAILED",
                "configuration",
                "Check the local Remote API NODE and PORT configuration and retain the \
                 native code; configuration failure does not identify target state.",
            ),
            "T32_Init" => (
                "API_INITIALIZATION_FAILED",
                "initialization",
                "Check that the configured endpoint is a compatible PowerView \
                 RCL=NETTCP service. A reachable TCP endpoint alone does not prove \
                 Remote API initialization succeeded.",
            ),
            "T32_Attach" => (
                "API_ATTACH_FAILED",
                "attach",
                "Inspect PowerView's Remote API debugger attachment and native error. \
                 Do not infer board power or issue a reset from this error alone.",
            ),
            _ => (
                "NATIVE_API_ERROR",
                stage,
                "Inspect the reported operation and native code in the context of the \
                 current target and PowerView configuration. This error alone does \
                 not establish a lost connection or an unsupported capability.",
            ),
        };
        let mut error = error_value(code, stage, message, hint);
        error["operation"] = json!(operation);
        error["native_code"] = json!(native_code);
        return error;
    }
    if message.starts_with("Running-target memory reads require access=arm_dap")
        || message.starts_with("Memory reads require an active target; TRACE32 reported state ")
    {
        return error_value(
            "TARGET_STATE_INCOMPATIBLE",
            stage,
            message,
            "Review the observed target state and explicitly selected access mode. \
             Do not automatically halt, resume, or reset the target to bypass this guard.",
        );
    }
    if message.starts_with("Symbol ") && message.ends_with(" was not found in loaded debug symbols")
    {
        return error_value(
            "SYMBOL_NOT_FOUND",
            stage,
            message,
            "Check the exact symbol name and intended debug-symbol image. A missing \
             symbol does not prove that no symbols are loaded.",
        );
    }
    if message.starts_with("T32_GetWindowContent failed;") {
        let mut error = error_value(
            "WINDOW_QUERY_FAILED",
            stage,
            message,
            "Inspect whether this window is available in the active CPU, debug-symbol \
             and OS-awareness configuration. Failure alone does not prove lack of support.",
        );
        error["operation"] = json!("T32_GetWindowContent");
        return error;
    }
    error_value(
        "UNKNOWN_ERROR",
        stage,
        message,
        "Retain this original error and inspect the surrounding evidence. \
         No specific cause or automatic recovery action has been established.",
    )
}

fn normalize_observation(field: &str, value: Option<&Value>) -> Value {
    let stage = format!("status.{field}");
    if let Some(value) = value {
        if value["ok"] == true
            && let Some(result) = value.get("value")
        {
            return json!({"ok": true, "value": result});
        }
        if value["ok"] == false
            && let Some(error) = value["error"].as_str()
        {
            return json!({"ok": false, "error": classify_error(&stage, error)});
        }
    }
    json!({
        "ok": false,
        "error": error_value(
            "INVALID_STATUS_OBSERVATION",
            &stage,
            "The status query did not provide a valid observation.",
            "Inspect the status response shape; no value or target state was inferred.",
        ),
    })
}

fn capability(observed: bool, evidence: &str) -> Value {
    json!({
        "status": if observed { "observed_success" } else { "unknown" },
        "evidence": evidence,
    })
}

fn target_status(observation: &Value) -> Value {
    let state = (observation["ok"] == true)
        .then(|| observation["value"].as_i64())
        .flatten();
    let (name, hint) = match state {
        Some(0) => (
            "down",
            "TRACE32 reports system down. Inspect the target connection and configuration \
             manually; this does not prove the board is unpowered.",
        ),
        Some(1) => (
            "halted",
            "TRACE32 reports system halted. No resume or other target-control action was taken.",
        ),
        Some(2) => (
            "stopped",
            "TRACE32 reports emulation stopped. No resume or other target-control action was taken.",
        ),
        Some(3) => (
            "running",
            "CPU register reads may be unavailable while running. This server only \
             permits running-target memory requests with explicit arm_dap and \
             dual_port=true on compatible targets. ARM DAP is not a portable \
             fallback; doctor has not established support for this access mode.",
        ),
        _ => (
            "unknown",
            "No recognized target state was observed. Do not infer target power, \
             halted state, or safe memory access from this report.",
        ),
    };
    let mut target = json!({"state": name, "actionable_hint": hint});
    if let Some(state) = state {
        target["native_state"] = json!(state);
    }
    target
}

/// Diagnose a caller-owned connection with status and configured metadata queries.
/// Even connection failures are returned as useful structured reports.
pub fn doctor(port: u16, connection_result: Result<(), String>, per_support: PerSupport) -> Value {
    doctor_with_metadata(
        port,
        connection_result,
        per_support,
        crate::inspection::status,
        crate::t32_eval,
    )
}

fn doctor_with_metadata(
    port: u16,
    connection_result: Result<(), String>,
    per_support: PerSupport,
    status: impl FnOnce(u16) -> Value,
    evaluate: impl FnOnce(&str) -> Result<crate::Evaluation, String>,
) -> Value {
    let connected = connection_result.is_ok();
    let mut report = doctor_with(port, connection_result, status);
    let per = capabilities::per_report_with(per_support, connected, evaluate);
    let definition = &per["active_definition"];
    if definition["status"] != "not_attempted" {
        let succeeded = matches!(definition["status"].as_str(), Some("present" | "absent"));
        for count in ["attempted", if succeeded { "succeeded" } else { "failed" }] {
            report["queries"][count] = json!(report["queries"][count].as_u64().unwrap() + 1);
        }
        if succeeded {
            report["capabilities"]["remote_api"] = capability(
                true,
                "At least one status or metadata query succeeded during this report.",
            );
        } else {
            let error = if let Some(error) = definition["error"].as_str() {
                classify_error("per_metadata", error)
            } else {
                error_value(
                    "INVALID_PER_METADATA",
                    "per_metadata",
                    definition["reason"]
                        .as_str()
                        .unwrap_or("No usable PER metadata"),
                    "Inspect the current session's configuration; this does not establish that PER is unsupported.",
                )
            };
            report["errors"].as_array_mut().unwrap().push(error);
            report["outcome"] = json!("partially_observed");
        }
    }
    report["capabilities"]["per"] = per;
    report
}

/// Callback form for deterministic offline verification; the callback is invoked
/// exactly once on connection success and never on connection failure.
pub fn doctor_with(
    port: u16,
    connection_result: Result<(), String>,
    status: impl FnOnce(u16) -> Value,
) -> Value {
    let mut observations = Map::new();
    let mut errors = Vec::new();
    let (connection, attempted) = match connection_result {
        Ok(()) => {
            let status = status(port);
            for field in STATUS_FIELDS {
                let observation = normalize_observation(field, status.get(field));
                if let Some(error) = observation.get("error") {
                    errors.push(error.clone());
                }
                observations.insert(field.to_string(), observation);
            }
            (
                json!({
                    "ok": true,
                    "status": "established_or_reused",
                    "note": "A caller-owned connection was established or reused. \
                             Current API responsiveness is evidenced by the individual queries.",
                }),
                STATUS_FIELDS.len(),
            )
        }
        Err(message) => {
            let error = classify_error("connection", &message);
            errors.push(error.clone());
            for field in STATUS_FIELDS {
                observations.insert(
                    field.to_string(),
                    json!({
                        "ok": false,
                        "status": "not_attempted",
                        "reason": "connection_failed",
                    }),
                );
            }
            (json!({"ok": false, "status": "failed", "error": error}), 0)
        }
    };
    let succeeded = observations
        .values()
        .filter(|observation| observation["ok"] == true)
        .count();
    let target = target_status(&observations["state"]);
    let mut capabilities = Map::new();
    capabilities.insert(
        "remote_api".to_string(),
        capability(
            succeeded > 0,
            if succeeded > 0 {
                "At least one standard status query succeeded during this report."
            } else {
                "No successful status query established current API responsiveness."
            },
        ),
    );
    for field in STATUS_FIELDS {
        let observed = observations[field]["ok"] == true;
        capabilities.insert(
            field.to_string(),
            capability(
                observed,
                if observed {
                    "The corresponding status query returned a value."
                } else {
                    "No successful value was observed; support is not established."
                },
            ),
        );
    }
    for name in [
        "memory_read",
        "register_read",
        "symbol_lookup",
        "debugger_windows",
        "arbitrary_expression_evaluation",
    ] {
        capabilities.insert(
            name.to_string(),
            capability(false, "Not probed by this passive diagnostic."),
        );
    }
    capabilities.insert(
        "per".to_string(),
        capability(
            false,
            "PER metadata is not probed by status-only diagnostics.",
        ),
    );
    json!({
        "schema_version": 1,
        "endpoint": {"transport": "tcp", "host": "127.0.0.1", "port": port},
        "outcome": if attempted == 0 {
            "connection_failed"
        } else if errors.is_empty() {
            "observed"
        } else {
            "partially_observed"
        },
        "connection": connection,
        "target": target,
        "queries": {
            "attempted": attempted,
            "succeeded": succeeded,
            "failed": attempted - succeeded,
        },
        "observations": observations,
        "capabilities": capabilities,
        "errors": errors,
        "limitations": [
            "The TCP preflight is bounded; synchronous native calls have no hard deadline and can block.",
            "Status queries are sequential observations, not an atomic snapshot.",
            "No retries, reconnects, target memory probes, halt, resume, reset or image loads are performed by this diagnostic.",
            "Advertised MCP tools, CPU identity and a PER filename do not establish successful PER reads or memory access modes.",
            "Unknown capability status means unverified, not unsupported.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn good_status(state: i32) -> Value {
        json!({
            "state": {"ok": true, "value": state},
            "practice_state": {"ok": true, "value": 0},
            "software_version": {"ok": true, "value": "R.test"},
            "build": {"ok": true, "value": "123."},
            "cpu": {"ok": true, "value": "TestCpu"},
        })
    }

    #[test]
    fn absent_endpoint_returns_report_without_queries() {
        let result = doctor_with(
            20000,
            Err("Cannot reach TRACE32 at 127.0.0.1:20000: connection refused".into()),
            |_| panic!("Status must not be queried after a failed connection"),
        );
        assert_eq!(result["outcome"], "connection_failed");
        assert_eq!(result["connection"]["ok"], false);
        assert_eq!(
            result["connection"]["error"]["code"],
            "TCP_PREFLIGHT_FAILED"
        );
        assert_eq!(result["queries"]["attempted"], 0);
        assert_eq!(result["queries"]["failed"], 0);
        assert_eq!(result["target"]["state"], "unknown");
        assert!(result["target"].get("native_state").is_none());
        assert_eq!(result["errors"].as_array().unwrap().len(), 1);
        for field in STATUS_FIELDS {
            assert_eq!(result["observations"][field]["status"], "not_attempted");
        }
    }

    #[test]
    fn status_callback_is_called_once_with_configured_port() {
        let calls = Cell::new(0);
        let result = doctor_with(20001, Ok(()), |port| {
            assert_eq!(port, 20001);
            calls.set(calls.get() + 1);
            good_status(3)
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(result["outcome"], "observed");
        assert_eq!(result["connection"]["status"], "established_or_reused");
        assert_eq!(result["queries"]["succeeded"], 5);
        assert_eq!(
            result["capabilities"]["remote_api"]["status"],
            "observed_success"
        );
        for name in [
            "memory_read",
            "register_read",
            "symbol_lookup",
            "debugger_windows",
            "arbitrary_expression_evaluation",
            "per",
        ] {
            assert_eq!(result["capabilities"][name]["status"], "unknown");
        }
    }

    #[test]
    fn cpu_identity_and_stopped_state_do_not_establish_per_support() {
        for cpu in [
            Some("CortexM0+"),
            Some("RH850"),
            Some("RISC-V"),
            Some("UnknownCpu"),
            None,
        ] {
            for state in [1, 2, 3] {
                let calls = Cell::new(0);
                let result = doctor_with(20000, Ok(()), |_| {
                    calls.set(calls.get() + 1);
                    let mut status = good_status(state);
                    status["cpu"] = match cpu {
                        Some(cpu) => json!({"ok": true, "value": cpu}),
                        None => json!({"ok": false, "error": "CPU query unavailable"}),
                    };
                    status
                });
                assert_eq!(calls.get(), 1);
                assert_eq!(result["queries"]["attempted"], STATUS_FIELDS.len());
                assert_eq!(result["observations"]["cpu"]["ok"], cpu.is_some());
                for name in ["per", "memory_read"] {
                    assert_eq!(result["capabilities"][name]["status"], "unknown");
                }
            }
        }
    }

    #[test]
    fn doctor_includes_metadata_queries_in_its_overall_outcome() {
        for (policy, fail, attempts, succeeded, failed, outcome) in [
            (PerSupport::Auto, false, 6, 6, 0, "observed"),
            (PerSupport::Auto, true, 6, 5, 1, "partially_observed"),
            (PerSupport::Supported, true, 6, 5, 1, "partially_observed"),
            (PerSupport::Unsupported, false, 5, 5, 0, "observed"),
        ] {
            let calls = Cell::new(0);
            let result = doctor_with_metadata(
                20000,
                Ok(()),
                policy,
                |_| good_status(3),
                |expression| {
                    calls.set(calls.get() + 1);
                    assert_eq!(expression, "PER.FILENAME()");
                    if fail {
                        Err("T32_ExecuteFunction failed (Remote API error 1)".into())
                    } else {
                        Ok(crate::Evaluation {
                            value: "session.per".into(),
                            result_type: 0x0040,
                        })
                    }
                },
            );
            assert_eq!(calls.get(), usize::from(policy != PerSupport::Unsupported));
            assert_eq!(result["outcome"], outcome);
            assert_eq!(result["queries"]["attempted"], attempts);
            assert_eq!(result["queries"]["succeeded"], succeeded);
            assert_eq!(result["queries"]["failed"], failed);
            assert_eq!(result["errors"].as_array().unwrap().len(), failed);
            if fail {
                assert_eq!(result["errors"][0]["stage"], "per_metadata");
            }
        }
    }

    #[test]
    fn invalid_metadata_is_not_counted_as_successful_diagnostics() {
        let result = doctor_with_metadata(
            20000,
            Ok(()),
            PerSupport::Auto,
            |_| good_status(2),
            |_| {
                Ok(crate::Evaluation {
                    value: "0".into(),
                    result_type: 8,
                })
            },
        );
        assert_eq!(result["outcome"], "partially_observed");
        assert_eq!(result["queries"]["failed"], 1);
        assert_eq!(result["errors"][0]["code"], "INVALID_PER_METADATA");
        assert_eq!(result["capabilities"]["per"]["status"], "unknown");
    }

    #[test]
    fn optional_status_failures_do_not_discard_other_results() {
        let result = doctor_with(20000, Ok(()), |_| {
            let mut status = good_status(2);
            status["cpu"] = json!({
                "ok": false,
                "error": "T32_ExecuteFunction failed (Remote API error 4113)",
            });
            status["build"] = json!({"ok": false, "error": "unknown build metadata failure"});
            status
        });
        assert_eq!(result["outcome"], "partially_observed");
        assert_eq!(result["target"]["state"], "stopped");
        assert_eq!(
            result["observations"]["software_version"]["value"],
            "R.test"
        );
        assert_eq!(result["queries"]["succeeded"], 3);
        assert_eq!(result["queries"]["failed"], 2);
        assert_eq!(
            result["observations"]["cpu"]["error"]["code"],
            "NATIVE_API_ERROR"
        );
        assert_eq!(result["observations"]["cpu"]["error"]["native_code"], 4113);
        assert_eq!(
            result["observations"]["cpu"]["error"]["stage"],
            "status.cpu"
        );
        assert_eq!(
            result["observations"]["build"]["error"]["code"],
            "UNKNOWN_ERROR"
        );
        assert_eq!(result["capabilities"]["cpu"]["status"], "unknown");
        assert_eq!(
            result["capabilities"]["remote_api"]["status"],
            "observed_success"
        );
    }

    #[test]
    fn successful_connection_does_not_fabricate_current_api_health() {
        let result = doctor_with(20000, Ok(()), |_| {
            let mut status = Map::new();
            for field in STATUS_FIELDS {
                status.insert(
                    field.into(),
                    json!({"ok": false, "error": "No response from query"}),
                );
            }
            Value::Object(status)
        });
        assert_eq!(result["connection"]["ok"], true);
        assert_eq!(result["queries"]["failed"], 5);
        assert_eq!(result["capabilities"]["remote_api"]["status"], "unknown");
        assert_eq!(result["target"]["state"], "unknown");
    }

    #[test]
    fn target_states_are_mapped_without_inferring_board_power() {
        for (native, expected) in [
            (0, "down"),
            (1, "halted"),
            (2, "stopped"),
            (3, "running"),
            (-1, "unknown"),
            (4, "unknown"),
        ] {
            let result = doctor_with(20000, Ok(()), |_| good_status(native));
            assert_eq!(result["target"]["state"], expected);
            assert_eq!(result["target"]["native_state"], native);
        }
    }

    #[test]
    fn missing_or_malformed_fields_are_not_observations() {
        let result = doctor_with(20000, Ok(()), |_| {
            json!({
                "state": {"ok": true},
                "practice_state": {"ok": "true", "value": 0},
                "cpu": {"ok": false, "error": 123},
                "software_version": {"ok": true, "value": "R.test"},
            })
        });
        assert_eq!(result["queries"]["succeeded"], 1);
        assert_eq!(result["queries"]["failed"], 4);
        assert_eq!(result["target"]["state"], "unknown");
        for field in ["state", "practice_state", "cpu", "build"] {
            assert_eq!(
                result["observations"][field]["error"]["code"],
                "INVALID_STATUS_OBSERVATION"
            );
        }
    }

    #[test]
    fn malformed_target_state_never_becomes_halted_or_down() {
        for value in [json!(null), json!("0"), json!(true), json!(0.5)] {
            let result = doctor_with(20000, Ok(()), |_| {
                let mut status = good_status(2);
                status["state"]["value"] = value;
                status
            });
            assert_eq!(result["target"]["state"], "unknown");
            assert!(result["target"].get("native_state").is_none());
        }
    }

    #[test]
    fn connection_stages_and_native_codes_remain_distinct() {
        for (message, code, stage, operation, native_code) in [
            (
                "Connection with TRACE32 failed: T32_RequestChannelNetTcp returned -1",
                "CHANNEL_CREATE_FAILED",
                "channel",
                "T32_RequestChannelNetTcp",
                -1,
            ),
            (
                "T32_Config failed (Remote API error 2)",
                "API_CONFIGURATION_FAILED",
                "configuration",
                "T32_Config",
                2,
            ),
            (
                "T32_Init failed (Remote API error -3)",
                "API_INITIALIZATION_FAILED",
                "initialization",
                "T32_Init",
                -3,
            ),
            (
                "T32_Attach failed (Remote API error 4)",
                "API_ATTACH_FAILED",
                "attach",
                "T32_Attach",
                4,
            ),
        ] {
            let error = classify_error("connection", message);
            assert_eq!(error["code"], code);
            assert_eq!(error["stage"], stage);
            assert_eq!(error["operation"], operation);
            assert_eq!(error["native_code"], native_code);
            assert_eq!(error["message"], message);
        }
    }

    #[test]
    fn query_error_retains_native_operation_and_caller_stage() {
        let error = classify_error(
            "read_memory",
            "T32_ReadMemoryObj failed (Remote API error 6)",
        );
        assert_eq!(error["code"], "NATIVE_API_ERROR");
        assert_eq!(error["stage"], "read_memory");
        assert_eq!(error["operation"], "T32_ReadMemoryObj");
        assert_eq!(error["native_code"], 6);
        assert!(!error["actionable_hint"].as_str().unwrap().contains("retry"));
    }

    #[test]
    fn unknown_error_does_not_extract_unrelated_numbers_or_guess_cause() {
        for message in [
            "timeout after 3000 ms, Windows error 10061",
            "wrapped T32_Init failed (Remote API error 7)",
            "T32_Init failed (Remote API error 7) with extra detail",
            "T32_Init failed (Remote API error not-a-number)",
            "T32_Init failed (Remote API error 2147483648)",
        ] {
            let error = classify_error("query", message);
            assert_eq!(error["code"], "UNKNOWN_ERROR");
            assert_eq!(error["stage"], "query");
            assert_eq!(error["message"], message);
            assert!(error.get("operation").is_none());
            assert!(error.get("native_code").is_none());
        }
    }

    #[test]
    fn non_native_known_errors_keep_narrow_categories() {
        for (message, code) in [
            (
                "TRACE32 TCP port must be between 1 and 65535",
                "INVALID_CONFIGURATION",
            ),
            (
                "No TRACE32 connection on this thread; connect before querying the native API",
                "NOT_CONNECTED",
            ),
            (
                "Symbol \"missing\" was not found in loaded debug symbols",
                "SYMBOL_NOT_FOUND",
            ),
            (
                "Running-target memory reads require access=arm_dap and dual_port=true (EDAP).",
                "TARGET_STATE_INCOMPATIBLE",
            ),
            (
                "Memory reads require an active target; TRACE32 reported state 0",
                "TARGET_STATE_INCOMPATIBLE",
            ),
            (
                "T32_GetWindowContent failed; this window may not be supported by the active PowerView configuration",
                "WINDOW_QUERY_FAILED",
            ),
        ] {
            let error = classify_error("read", message);
            assert_eq!(error["code"], code);
            assert!(error.get("native_code").is_none());
        }
    }
}
