// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Bounded queries against the default TRACE32 PER definition. PER functions may
//! interpret dynamic instructions, including writes: this is not a read-only
//! interpreter or an MMIO safety boundary. Callers validate before connecting
//! and serialize API access. This module never connects, halts, or resumes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_PATHS: usize = 64;
const MAX_PATH_BYTES: usize = 512;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_RETAINED_VALUE_BYTES: usize = 65536;
const MAX_ERROR_BYTES: usize = 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TIMEOUT_MS: u64 = 60000;

fn default_include_value() -> bool {
    true
}

fn default_timeout_ms() -> u64 {
    5000
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadPerRegistersRequest {
    /// Exact, case-sensitive paths in the default PER definition, from 1 through
    /// 64. A leading dot selects a register without its group path. No wildcards.
    #[schemars(length(min = 1, max = 64))]
    pub paths: Vec<String>,
    /// Evaluate PER.VALUE as well as PER.ADDRESS for each valid path.
    #[serde(default = "default_include_value")]
    pub include_value: bool,
    /// Also evaluate PER.VALUE.STRING, which applies to named BITFLD choices.
    /// Failure of this optional field does not discard a successfully read value.
    #[serde(default)]
    pub include_choice: bool,
    /// Required even for address-only requests: PER definitions may execute
    /// dynamic instructions or writes, and peripheral reads may have side effects.
    #[serde(default)]
    pub acknowledge_side_effects: bool,
    /// Soft deadline, from 1 through 60000 ms, checked between native calls.
    /// An in-progress native call cannot be interrupted by this deadline.
    #[serde(default = "default_timeout_ms")]
    #[schemars(range(min = 1, max = 60000))]
    pub timeout_ms: u64,
}

impl ReadPerRegistersRequest {
    /// Validate the entire batch, including the acknowledgement, before any
    /// connection, state probe, or expression evaluation.
    pub fn validate(&self) -> Result<(), String> {
        if self.paths.is_empty() || self.paths.len() > MAX_PATHS {
            return Err(format!(
                "paths must contain between 1 and {MAX_PATHS} items"
            ));
        }
        let mut seen = HashSet::new();
        for (index, path) in self.paths.iter().enumerate() {
            validate_path(path, index)?;
            if !seen.insert(path) {
                return Err(format!("paths[{index}] duplicates an earlier PER path"));
            }
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(format!("timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"));
        }
        if !self.acknowledge_side_effects {
            return Err(
                "acknowledge_side_effects=true is required: PER definitions can execute dynamic instructions or writes, including during address lookup; peripheral reads may have side effects"
                    .into(),
            );
        }
        Ok(())
    }
}

fn validate_path(path: &str, index: usize) -> Result<(), String> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.trim() != path {
        return Err(format!(
            "paths[{index}] must contain 1..{MAX_PATH_BYTES} ASCII bytes without leading or trailing spaces"
        ));
    }
    // Quoted arguments are generated here, never supplied by callers.
    // Excluding macro/escape/command syntax also prevents interpolation.
    if !path.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'.' | b' ' | b'[' | b']' | b':' | b'-' | b'(' | b')'
            )
    }) || !path
        .bytes()
        .any(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!(
            "paths[{index}] must use only ASCII letters, digits, underscore, dot, space, brackets, parentheses, colon, or hyphen; only literal path text is accepted"
        ));
    }
    Ok(())
}

/// Injectable native operations; implementations must not connect implicitly.
pub trait PeripheralBackend {
    fn state(&mut self) -> Result<i32, String>;
    fn evaluate(&mut self, expression: &str) -> Result<crate::Evaluation, String>;
}

pub struct NativePeripheralBackend;

impl PeripheralBackend for NativePeripheralBackend {
    fn state(&mut self) -> Result<i32, String> {
        crate::t32_state()
    }

    fn evaluate(&mut self, expression: &str) -> Result<crate::Evaluation, String> {
        crate::t32_eval(expression)
    }
}

pub fn read_registers(request: &ReadPerRegistersRequest) -> Result<Value, String> {
    read_registers_with_backend(request, &mut NativePeripheralBackend)
}

pub fn read_registers_with_backend(
    request: &ReadPerRegistersRequest,
    backend: &mut impl PeripheralBackend,
) -> Result<Value, String> {
    let started = Instant::now();
    read_registers_with_clock(request, backend, &mut || started.elapsed())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn bounded_error(message: &str) -> (String, bool) {
    if message.len() <= MAX_ERROR_BYTES {
        return (message.to_string(), false);
    }
    let suffix = "... [truncated]";
    let mut end = MAX_ERROR_BYTES - suffix.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{suffix}", &message[..end]), true)
}

fn diagnostic(code: &str, message: &str) -> Value {
    let (message, truncated) = bounded_error(message);
    json!({
        "code": code,
        "message": message,
        "message_truncated": truncated,
    })
}

fn not_attempted(reason_code: &str) -> Value {
    json!({"status": "not_attempted", "ok": false, "reason_code": reason_code})
}

fn not_requested() -> Value {
    json!({"status": "not_requested"})
}

fn error_field(code: &str, message: &str) -> Value {
    json!({"status": "error", "ok": false, "error": diagnostic(code, message)})
}

struct Reader<'a, B, F> {
    request: &'a ReadPerRegistersRequest,
    backend: &'a mut B,
    elapsed: &'a mut F,
    baseline: Option<i32>,
    state_integrity_lost: bool,
    first_state: Option<Value>,
    last_state: Option<Value>,
    stop_reason: Option<Value>,
    retained_value_bytes: usize,
    evaluations: usize,
    state_probes: usize,
}

impl<B: PeripheralBackend, F: FnMut() -> Duration> Reader<'_, B, F> {
    fn stop(&mut self, reason: Value) {
        if self.stop_reason.is_none() {
            self.stop_reason = Some(reason);
        }
    }

    fn stop_code(&self) -> &str {
        self.stop_reason
            .as_ref()
            .and_then(|reason| reason["code"].as_str())
            .unwrap_or("BATCH_STOPPED")
    }

    fn within_deadline(&mut self) -> bool {
        if (self.elapsed)() >= Duration::from_millis(self.request.timeout_ms) {
            self.stop(diagnostic(
                "DEADLINE_EXCEEDED",
                "The soft deadline was reached between native calls; no further call was started. A native call already in progress cannot be interrupted.",
            ));
            false
        } else {
            true
        }
    }

    fn probe_state(&mut self) -> Value {
        if !self.within_deadline() {
            return not_attempted("DEADLINE_EXCEEDED");
        }
        self.state_probes += 1;
        let observation = match self.backend.state() {
            Ok(state) => {
                if !matches!(state, 1 | 2) {
                    self.state_integrity_lost = true;
                    let mut reason = diagnostic(
                        "TARGET_STATE_UNSAFE",
                        "PER evaluation requires an already stopped target (state 1 or 2); the tool will not halt, resume, or reset it.",
                    );
                    reason["observed_state"] = json!(state);
                    self.stop(reason);
                } else if self.baseline.is_some_and(|baseline| baseline != state) {
                    self.state_integrity_lost = true;
                    let mut reason = diagnostic(
                        "TARGET_STATE_CHANGED",
                        "Target state changed during the batch; no further PER expression will be evaluated.",
                    );
                    reason["expected_state"] = json!(self.baseline);
                    reason["observed_state"] = json!(state);
                    self.stop(reason);
                } else if self.baseline.is_none() {
                    self.baseline = Some(state);
                }
                json!({
                    "status": "ok",
                    "ok": true,
                    "value": state,
                    "observed_unix_ms": unix_ms(),
                })
            }
            Err(error) => {
                self.state_integrity_lost = true;
                self.stop(diagnostic("STATE_QUERY_FAILED", &error));
                let mut observation = error_field("STATE_QUERY_FAILED", &error);
                observation["observed_unix_ms"] = json!(unix_ms());
                observation
            }
        };
        if self.first_state.is_none() {
            self.first_state = Some(observation.clone());
        }
        self.last_state = Some(observation.clone());
        self.within_deadline();
        observation
    }

    fn evaluate(&mut self, expression: &str) -> (Value, Option<Value>) {
        if self.stop_reason.is_some() {
            return (not_attempted(self.stop_code()), None);
        }
        let state_before = self.probe_state();
        if self.stop_reason.is_some() {
            return (not_attempted(self.stop_code()), Some(state_before));
        }
        // A state probe can itself consume the remaining soft deadline.
        if !self.within_deadline() {
            return (not_attempted(self.stop_code()), Some(state_before));
        }
        self.evaluations += 1;
        let field = match self.backend.evaluate(expression) {
            Ok(result) if result.value.len() > MAX_VALUE_BYTES => error_field(
                "VALUE_TOO_LARGE",
                &format!(
                    "Native value contains {} bytes, exceeding the {MAX_VALUE_BYTES}-byte field limit; it was not retained or silently truncated.",
                    result.value.len()
                ),
            ),
            Ok(result)
                if self.retained_value_bytes + result.value.len() > MAX_RETAINED_VALUE_BYTES =>
            {
                self.stop(diagnostic(
                    "OUTPUT_LIMIT_REACHED",
                    "The batch reached its retained-value byte limit; no further PER expression will be evaluated.",
                ));
                error_field(
                    "OUTPUT_LIMIT_REACHED",
                    "Native value was not retained because the batch value budget would be exceeded.",
                )
            }
            Ok(result) => {
                self.retained_value_bytes += result.value.len();
                json!({
                    "status": "ok",
                    "ok": true,
                    "value": result.value,
                    "result_type": result.result_type,
                })
            }
            Err(error) => error_field("NATIVE_EVALUATION_FAILED", &error),
        };
        self.within_deadline();
        (field, Some(state_before))
    }

    fn read_path(&mut self, path: &str) -> Value {
        if self.stop_reason.is_some() {
            return empty_item(path, self.request, self.stop_code());
        }
        let item_started_unix_ms = unix_ms();
        let item_started = (self.elapsed)();
        let evaluations_before = self.evaluations;
        let mut item = empty_item(path, self.request, "NOT_STARTED");
        item["started_unix_ms"] = json!(item_started_unix_ms);
        let (address, state_before) = self.evaluate(&format!("PER.ADDRESS(\"{path}\")"));
        if let Some(state_before) = state_before {
            item["state_before"] = state_before;
        }
        item["address"] = address;
        self.read_optional_fields(path, &mut item);
        self.finish_item(&mut item, self.evaluations > evaluations_before);
        item["completed_unix_ms"] = json!(unix_ms());
        item["elapsed_ms"] = json!(duration_ms((self.elapsed)().saturating_sub(item_started)));
        item
    }

    fn read_optional_fields(&mut self, path: &str, item: &mut Value) {
        for (requested, field, function) in [
            (self.request.include_value, "value", "PER.VALUE"),
            (self.request.include_choice, "choice", "PER.VALUE.STRING"),
        ] {
            if !requested {
                continue;
            }
            item[field] = if self.stop_reason.is_some() {
                not_attempted(self.stop_code())
            } else if item["address"]["ok"] != true {
                // Avoid re-interpreting an invalid path through additional functions.
                not_attempted("ADDRESS_FAILED")
            } else {
                self.evaluate(&format!("{function}(\"{path}\")")).0
            };
        }
    }

    fn finish_item(&mut self, item: &mut Value, attempted: bool) {
        if attempted {
            // Post-item state is evidence, not permission to retry or recover.
            item["state_after"] = self.probe_state();
        } else {
            item["state_after"] = not_attempted(if self.stop_reason.is_some() {
                self.stop_code()
            } else {
                "NO_PER_EVALUATION"
            });
        }
        let stable = attempted
            && !self.state_integrity_lost
            && item["state_after"]["ok"] == true
            && item["state_after"]["value"].as_i64() == self.baseline.map(i64::from);
        item["state_stable"] = json!(stable);
        let mut requested_fields = vec!["address"];
        if self.request.include_value {
            requested_fields.push("value");
        }
        if self.request.include_choice {
            requested_fields.push("choice");
        }
        let successful_fields = requested_fields
            .iter()
            .filter(|field| item[**field]["ok"] == true)
            .count();
        let status = if successful_fields == requested_fields.len() && stable {
            "ok"
        } else if successful_fields > 0 {
            "partial"
        } else if attempted {
            "error"
        } else {
            "not_attempted"
        };
        item["status"] = json!(status);
        item["ok"] = json!(status == "ok");
    }
}

fn empty_item(path: &str, request: &ReadPerRegistersRequest, reason_code: &str) -> Value {
    json!({
        "path": path,
        "status": "not_attempted",
        "ok": false,
        "started_unix_ms": null,
        "completed_unix_ms": null,
        "elapsed_ms": 0,
        "state_before": not_attempted(reason_code),
        "state_after": not_attempted(reason_code),
        "state_stable": false,
        "address": not_attempted(reason_code),
        "value": if request.include_value { not_attempted(reason_code) } else { not_requested() },
        "choice": if request.include_choice { not_attempted(reason_code) } else { not_requested() },
    })
}

fn read_registers_with_clock(
    request: &ReadPerRegistersRequest,
    backend: &mut impl PeripheralBackend,
    elapsed: &mut impl FnMut() -> Duration,
) -> Result<Value, String> {
    request.validate()?;
    let started_unix_ms = unix_ms();
    let mut reader = Reader {
        request,
        backend,
        elapsed,
        baseline: None,
        state_integrity_lost: false,
        first_state: None,
        last_state: None,
        stop_reason: None,
        retained_value_bytes: 0,
        evaluations: 0,
        state_probes: 0,
    };
    let results = request
        .paths
        .iter()
        .map(|path| reader.read_path(path))
        .collect();
    let elapsed_ms = duration_ms((reader.elapsed)());
    finish_response(
        request,
        started_unix_ms,
        elapsed_ms,
        if reader.stop_reason.is_some() {
            "stopped"
        } else {
            "completed"
        },
        results,
        reader.first_state,
        reader.last_state,
        reader.stop_reason,
        reader.evaluations,
        reader.state_probes,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_response(
    request: &ReadPerRegistersRequest,
    started_unix_ms: u64,
    elapsed_ms: u64,
    status: &str,
    results: Vec<Value>,
    state_before: Option<Value>,
    last_state: Option<Value>,
    stop_reason: Option<Value>,
    evaluations: usize,
    state_probes: usize,
) -> Result<Value, String> {
    let count = |status: &str| {
        results
            .iter()
            .filter(|item| item["status"] == status)
            .count()
    };
    let succeeded = count("ok");
    let response = json!({
        "format": "t32mcp.peripheral-registers",
        "version": 1,
        "status": status,
        "ok": succeeded == request.paths.len() && stop_reason.is_none(),
        "non_atomic": true,
        "definition_source": "default_per",
        "independent_evaluations": true,
        "warnings": [
            "Each address, value, and choice is an independent PER evaluation and may re-interpret definitions or re-read hardware; these fields are not a single atomic sample.",
            "PER definitions may execute dynamic instructions or writes. Peripheral reads can have side effects even while the target is stopped.",
            "Choice text is only applicable to BITFLD entries with named choices. Existing PER conditions are used unchanged; this tool does not issue PER.Set.CONDitions.",
        ],
        "acknowledged_side_effects": request.acknowledge_side_effects,
        "started_unix_ms": started_unix_ms,
        "completed_unix_ms": unix_ms(),
        "elapsed_ms": elapsed_ms,
        "timeout_ms": request.timeout_ms,
        "deadline_exceeded": elapsed_ms >= request.timeout_ms,
        "deadline_kind": "soft_between_native_calls",
        "state_before": state_before,
        "last_state_observation": last_state,
        "stop_reason": stop_reason,
        "counts": {
            "requested": request.paths.len(),
            "succeeded": succeeded,
            "partial": count("partial"),
            "failed": count("error"),
            "not_attempted": count("not_attempted"),
            "evaluations": evaluations,
            "state_probes": state_probes,
        },
        "results": results,
    });
    if serde_json::to_vec(&response)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_RESPONSE_BYTES
    {
        return Err("PER response exceeds the bounded serialized output limit".into());
    }
    Ok(response)
}

/// Preserve the requested paths as unattempted when the caller cannot connect.
/// This helper validates but never calls the native API.
pub fn connection_failure(request: &ReadPerRegistersRequest, error: &str) -> Result<Value, String> {
    unattempted_response(request, "connection_failed", "CONNECTION_FAILED", error)
}

/// Refuse a user-disabled capability without connecting or evaluating PER.
pub fn configured_unavailable(
    request: &ReadPerRegistersRequest,
    error: &str,
) -> Result<Value, String> {
    unattempted_response(request, "unavailable", "CONFIGURED_UNSUPPORTED", error)
}

fn unattempted_response(
    request: &ReadPerRegistersRequest,
    status: &str,
    code: &str,
    error: &str,
) -> Result<Value, String> {
    request.validate()?;
    let now = unix_ms();
    finish_response(
        request,
        now,
        0,
        status,
        request
            .paths
            .iter()
            .map(|path| empty_item(path, request, code))
            .collect(),
        None,
        None,
        Some(diagnostic(code, error)),
        0,
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    #[derive(Default)]
    struct FakeBackend {
        states: VecDeque<Result<i32, String>>,
        values: VecDeque<Result<crate::Evaluation, String>>,
        calls: Vec<String>,
        clock: Option<Rc<Cell<Duration>>>,
        advance_state: Duration,
        advance_evaluation: Duration,
    }

    impl PeripheralBackend for FakeBackend {
        fn state(&mut self) -> Result<i32, String> {
            self.calls.push("state".into());
            if let Some(clock) = &self.clock {
                clock.set(clock.get() + self.advance_state);
            }
            self.states.pop_front().unwrap_or(Ok(2))
        }

        fn evaluate(&mut self, expression: &str) -> Result<crate::Evaluation, String> {
            self.calls.push(expression.to_string());
            if let Some(clock) = &self.clock {
                clock.set(clock.get() + self.advance_evaluation);
            }
            self.values
                .pop_front()
                .unwrap_or_else(|| evaluation("0x1234", 1))
        }
    }

    fn evaluation(value: &str, result_type: u32) -> Result<crate::Evaluation, String> {
        Ok(crate::Evaluation {
            value: value.to_string(),
            result_type,
        })
    }

    fn request(paths: &[&str]) -> ReadPerRegistersRequest {
        ReadPerRegistersRequest {
            paths: paths.iter().map(|path| (*path).to_string()).collect(),
            include_value: true,
            include_choice: false,
            acknowledge_side_effects: true,
            timeout_ms: 5000,
        }
    }

    fn run(request: &ReadPerRegistersRequest, backend: &mut FakeBackend) -> Result<Value, String> {
        read_registers_with_clock(request, backend, &mut || Duration::ZERO)
    }

    #[test]
    fn defaults_require_explicit_acknowledgement() {
        let request: ReadPerRegistersRequest =
            serde_json::from_value(json!({"paths": [".STATUS"]})).unwrap();
        assert!(request.include_value);
        assert!(!request.include_choice);
        assert_eq!(request.timeout_ms, 5000);
        assert!(!request.acknowledge_side_effects);
        let mut backend = FakeBackend::default();
        assert!(run(&request, &mut backend).is_err());
        assert!(backend.calls.is_empty());
        assert!(connection_failure(&request, "offline").is_err());
    }

    #[test]
    fn address_only_requests_still_require_acknowledgement() {
        let mut request = request(&[".STATUS"]);
        request.include_value = false;
        request.acknowledge_side_effects = false;
        let mut backend = FakeBackend::default();
        assert!(run(&request, &mut backend).is_err());
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn validates_entire_batch_before_any_native_call() {
        let mut backend = FakeBackend::default();
        assert!(run(&request(&[".VALID", "bad;Go"]), &mut backend).is_err());
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn rejects_expression_macro_file_and_control_syntax() {
        for path in [
            "",
            " ",
            ".",
            "..",
            ".A\n",
            ".A\r",
            ".A\tB",
            ".A\0B",
            ".A;Go",
            ".A\"",
            ".A'",
            ".A\\",
            ".A/B",
            "&path",
            "${path}",
            "%path",
            "*.A",
            ".A?",
            ".A,B",
            ".A+1",
            ".A=1",
            ".A`x",
            ".A|x",
            ".A#x",
            ".A!x",
            ".A\u{7f}",
            ".A\u{200b}",
            ".A\u{00e9}",
            " .A",
            ".A ",
        ] {
            let mut backend = FakeBackend::default();
            assert!(
                run(&request(&[path]), &mut backend).is_err(),
                "accepted {path:?}"
            );
            assert!(backend.calls.is_empty());
        }
    }

    #[test]
    fn preserves_allowed_case_and_leading_dot_exactly() {
        let request = request(&[".RegisterName", "Clock Control.PORT-A[0]:Value_1"]);
        let mut backend = FakeBackend::default();
        let result = run(&request, &mut backend).unwrap();
        assert!(result["ok"].as_bool().unwrap());
        assert_eq!(result["results"][0]["path"], ".RegisterName");
        assert_eq!(
            backend.calls,
            [
                "state",
                "PER.ADDRESS(\".RegisterName\")",
                "state",
                "PER.VALUE(\".RegisterName\")",
                "state",
                "state",
                "PER.ADDRESS(\"Clock Control.PORT-A[0]:Value_1\")",
                "state",
                "PER.VALUE(\"Clock Control.PORT-A[0]:Value_1\")",
                "state",
            ]
        );
    }

    #[test]
    fn parenthesized_titles_and_function_like_names_remain_quoted_literals() {
        let request = request(&["Memory Protection Unit (MPU).MPU_TYPE", "PER.VALUE(.A)"]);
        let mut backend = FakeBackend::default();
        let result = run(&request, &mut backend).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(
            backend.calls[1],
            "PER.ADDRESS(\"Memory Protection Unit (MPU).MPU_TYPE\")"
        );
        assert_eq!(backend.calls[6], "PER.ADDRESS(\"PER.VALUE(.A)\")");
    }

    #[test]
    fn request_limits_are_checked_before_calls() {
        let mut invalid = vec![
            request(&[]),
            request(&[".A", ".A"]),
            request(&[&"A".repeat(MAX_PATH_BYTES + 1)]),
        ];
        let mut too_many = request(&[".A"]);
        too_many.paths = (0..=MAX_PATHS).map(|i| format!(".R{i}")).collect();
        invalid.push(too_many);
        for timeout_ms in [0, MAX_TIMEOUT_MS + 1, u64::MAX] {
            let mut value = request(&[".A"]);
            value.timeout_ms = timeout_ms;
            invalid.push(value);
        }
        for value in invalid {
            let mut backend = FakeBackend::default();
            assert!(run(&value, &mut backend).is_err());
            assert!(backend.calls.is_empty());
        }
        assert!(request(&[&"A".repeat(MAX_PATH_BYTES)]).validate().is_ok());
        assert!(request(&[".A", ".a"]).validate().is_ok());
    }

    #[test]
    fn rejects_unknown_request_fields() {
        assert!(
            serde_json::from_value::<ReadPerRegistersRequest>(json!({
                "paths": [".A"],
                "acknowledge_side_effects": true,
                "filename": "custom.per",
            }))
            .is_err()
        );
    }

    #[test]
    fn preserves_raw_native_text_and_type_per_field() {
        let mut request = request(&[".BITFLD"]);
        request.include_choice = true;
        let mut backend = FakeBackend {
            values: VecDeque::from([
                evaluation("D:0x40001000", 2),
                evaluation("1.", 8),
                evaluation("\"Enabled\"", 4),
            ]),
            ..Default::default()
        };
        let result = run(&request, &mut backend).unwrap();
        let item = &result["results"][0];
        assert_eq!(item["address"]["value"], "D:0x40001000");
        assert_eq!(item["address"]["result_type"], 2);
        assert_eq!(item["value"]["value"], "1.");
        assert_eq!(item["value"]["result_type"], 8);
        assert_eq!(item["choice"]["value"], "\"Enabled\"");
        assert_eq!(item["choice"]["result_type"], 4);
        assert_eq!(item["status"], "ok");
        assert_eq!(result["counts"]["evaluations"], 3);
        assert_eq!(result["counts"]["state_probes"], 4);
        assert_eq!(result["non_atomic"], true);
        assert_eq!(result["definition_source"], "default_per");
        assert_eq!(result["status"], "completed");
        assert_eq!(result["stop_reason"], Value::Null);
    }

    #[test]
    fn address_only_does_not_evaluate_optional_fields() {
        let mut request = request(&[".STATUS"]);
        request.include_value = false;
        let mut backend = FakeBackend::default();
        let result = run(&request, &mut backend).unwrap();
        assert_eq!(
            backend.calls,
            ["state", "PER.ADDRESS(\".STATUS\")", "state"]
        );
        assert_eq!(result["results"][0]["value"]["status"], "not_requested");
        assert_eq!(result["results"][0]["choice"]["status"], "not_requested");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn choice_can_be_requested_independently_of_numeric_value() {
        let mut request = request(&[".MODE"]);
        request.include_value = false;
        request.include_choice = true;
        let mut backend = FakeBackend::default();
        let result = run(&request, &mut backend).unwrap();
        assert_eq!(
            backend.calls,
            [
                "state",
                "PER.ADDRESS(\".MODE\")",
                "state",
                "PER.VALUE.STRING(\".MODE\")",
                "state",
            ]
        );
        assert_eq!(result["results"][0]["value"]["status"], "not_requested");
        assert_eq!(result["results"][0]["choice"]["ok"], true);
    }

    #[test]
    fn optional_choice_failure_keeps_successful_value_and_later_paths() {
        let mut request = request(&[".A", ".B"]);
        request.include_choice = true;
        let mut backend = FakeBackend {
            values: VecDeque::from([
                evaluation("0x10", 1),
                evaluation("7.", 8),
                Err("Not a BITFLD with choices".into()),
            ]),
            ..Default::default()
        };
        let result = run(&request, &mut backend).unwrap();
        assert_eq!(result["results"][0]["value"]["value"], "7.");
        assert_eq!(result["results"][0]["choice"]["status"], "error");
        assert_eq!(result["results"][0]["status"], "partial");
        assert_eq!(result["results"][1]["status"], "ok");
        assert_eq!(result["counts"]["partial"], 1);
        assert_eq!(result["counts"]["succeeded"], 1);
        assert_eq!(result["stop_reason"], Value::Null);
    }

    #[test]
    fn value_failure_does_not_prevent_independently_requested_choice() {
        let mut request = request(&[".MODE"]);
        request.include_choice = true;
        let mut backend = FakeBackend {
            values: VecDeque::from([
                evaluation("0x10", 1),
                Err("numeric value unavailable".into()),
                evaluation("Disabled", 4),
            ]),
            ..Default::default()
        };
        let result = run(&request, &mut backend).unwrap();
        assert_eq!(result["results"][0]["value"]["status"], "error");
        assert_eq!(result["results"][0]["choice"]["value"], "Disabled");
        assert_eq!(result["counts"]["evaluations"], 3);
    }

    #[test]
    fn address_failure_skips_optional_queries_without_stopping_other_paths() {
        let mut request = request(&[".Missing", ".Present"]);
        request.include_choice = true;
        let mut backend = FakeBackend {
            values: VecDeque::from([Err("PER path not found".into())]),
            ..Default::default()
        };
        let result = run(&request, &mut backend).unwrap();
        let missing = &result["results"][0];
        assert_eq!(missing["status"], "error");
        assert_eq!(missing["value"]["reason_code"], "ADDRESS_FAILED");
        assert_eq!(missing["choice"]["reason_code"], "ADDRESS_FAILED");
        assert_eq!(result["results"][1]["status"], "ok");
        assert_eq!(result["counts"]["evaluations"], 4);
        assert!(
            !backend
                .calls
                .iter()
                .any(|call| call == "PER.VALUE(\".Missing\")")
        );
    }

    #[test]
    fn refuses_running_down_or_unknown_state_without_evaluation() {
        for state in [0, 3, -1, 4, i32::MAX] {
            let mut backend = FakeBackend {
                states: VecDeque::from([Ok(state)]),
                ..Default::default()
            };
            let result = run(&request(&[".A", ".B"]), &mut backend).unwrap();
            assert_eq!(backend.calls, ["state"]);
            assert_eq!(result["status"], "stopped");
            assert_eq!(result["stop_reason"]["code"], "TARGET_STATE_UNSAFE");
            assert_eq!(result["stop_reason"]["observed_state"], state);
            assert_eq!(result["counts"]["not_attempted"], 2);
            assert_eq!(result["counts"]["evaluations"], 0);
        }
    }

    #[test]
    fn accepts_both_stopped_states_but_not_a_transition_between_them() {
        for state in [1, 2] {
            let mut backend = FakeBackend {
                states: VecDeque::from([Ok(state), Ok(state), Ok(state)]),
                ..Default::default()
            };
            assert_eq!(run(&request(&[".A"]), &mut backend).unwrap()["ok"], true);
        }
        let mut backend = FakeBackend {
            states: VecDeque::from([Ok(1), Ok(2), Ok(2)]),
            ..Default::default()
        };
        let result = run(&request(&[".A", ".B"]), &mut backend).unwrap();
        assert_eq!(result["stop_reason"]["code"], "TARGET_STATE_CHANGED");
        assert_eq!(result["counts"]["evaluations"], 1);
        assert_eq!(result["results"][0]["address"]["ok"], true);
        assert_eq!(result["results"][0]["value"]["status"], "not_attempted");
        assert_eq!(result["results"][1]["status"], "not_attempted");
    }

    #[test]
    fn state_is_checked_before_each_requested_expression() {
        let mut request = request(&[".A", ".B"]);
        request.include_choice = true;
        let mut backend = FakeBackend {
            states: VecDeque::from([Ok(2), Ok(2), Ok(3), Ok(3)]),
            ..Default::default()
        };
        let result = run(&request, &mut backend).unwrap();
        assert_eq!(result["counts"]["evaluations"], 2);
        assert_eq!(result["results"][0]["address"]["ok"], true);
        assert_eq!(result["results"][0]["value"]["ok"], true);
        assert_eq!(result["results"][0]["choice"]["status"], "not_attempted");
        assert_eq!(result["results"][0]["state_stable"], false);
        assert_eq!(result["results"][1]["status"], "not_attempted");
        assert!(
            !backend
                .calls
                .iter()
                .any(|call| call.starts_with("PER.VALUE.STRING"))
        );
    }

    #[test]
    fn post_item_state_change_preserves_values_and_stops_next_path() {
        let mut backend = FakeBackend {
            states: VecDeque::from([Ok(2), Ok(2), Ok(3)]),
            ..Default::default()
        };
        let result = run(&request(&[".A", ".B"]), &mut backend).unwrap();
        assert_eq!(result["results"][0]["address"]["ok"], true);
        assert_eq!(result["results"][0]["value"]["ok"], true);
        assert_eq!(result["results"][0]["state_after"]["value"], 3);
        assert_eq!(result["results"][0]["status"], "partial");
        assert_eq!(result["results"][1]["status"], "not_attempted");
        assert_eq!(result["counts"]["evaluations"], 2);
        assert_eq!(result["counts"]["state_probes"], 3);
    }

    #[test]
    fn state_error_before_first_expression_keeps_all_paths_unattempted() {
        let mut backend = FakeBackend {
            states: VecDeque::from([Err("T32_GetState failed (Remote API error 6)".into())]),
            ..Default::default()
        };
        let result = run(&request(&[".A", ".B"]), &mut backend).unwrap();
        assert_eq!(backend.calls, ["state"]);
        assert_eq!(result["stop_reason"]["code"], "STATE_QUERY_FAILED");
        assert_eq!(result["counts"]["not_attempted"], 2);
        assert_eq!(result["state_before"]["status"], "error");
    }

    #[test]
    fn post_item_state_error_does_not_erase_successful_reads() {
        let mut backend = FakeBackend {
            states: VecDeque::from([Ok(2), Ok(2), Err("lost state".into())]),
            ..Default::default()
        };
        let result = run(&request(&[".A", ".B"]), &mut backend).unwrap();
        assert_eq!(result["results"][0]["address"]["ok"], true);
        assert_eq!(result["results"][0]["value"]["ok"], true);
        assert_eq!(result["results"][0]["state_after"]["status"], "error");
        assert_eq!(result["results"][0]["status"], "partial");
        assert_eq!(result["results"][1]["status"], "not_attempted");
    }

    #[test]
    fn soft_deadline_before_first_call_performs_no_native_operation() {
        let request = request(&[".A"]);
        let mut backend = FakeBackend::default();
        let result = read_registers_with_clock(&request, &mut backend, &mut || {
            Duration::from_millis(request.timeout_ms)
        })
        .unwrap();
        assert!(backend.calls.is_empty());
        assert_eq!(result["stop_reason"]["code"], "DEADLINE_EXCEEDED");
        assert_eq!(result["deadline_exceeded"], true);
        assert_eq!(result["counts"]["not_attempted"], 1);
    }

    #[test]
    fn slow_state_call_prevents_expression_after_deadline() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut backend = FakeBackend {
            clock: Some(clock.clone()),
            advance_state: Duration::from_millis(5000),
            ..Default::default()
        };
        let result =
            read_registers_with_clock(&request(&[".A", ".B"]), &mut backend, &mut || clock.get())
                .unwrap();
        assert_eq!(backend.calls, ["state"]);
        assert_eq!(result["counts"]["evaluations"], 0);
        assert_eq!(result["counts"]["not_attempted"], 2);
        assert_eq!(result["stop_reason"]["code"], "DEADLINE_EXCEEDED");
    }

    #[test]
    fn slow_expression_retains_value_but_starts_no_further_native_call() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut backend = FakeBackend {
            clock: Some(clock.clone()),
            advance_evaluation: Duration::from_millis(6000),
            ..Default::default()
        };
        let result =
            read_registers_with_clock(&request(&[".A", ".B"]), &mut backend, &mut || clock.get())
                .unwrap();
        assert_eq!(backend.calls, ["state", "PER.ADDRESS(\".A\")"]);
        assert_eq!(result["results"][0]["address"]["ok"], true);
        assert_eq!(result["results"][0]["value"]["status"], "not_attempted");
        assert_eq!(
            result["results"][0]["state_after"]["status"],
            "not_attempted"
        );
        assert_eq!(result["results"][0]["state_stable"], false);
        assert_eq!(result["results"][1]["status"], "not_attempted");
        assert_eq!(result["elapsed_ms"], 6000);
        assert_eq!(result["deadline_exceeded"], true);
    }

    #[test]
    fn address_failure_at_deadline_preserves_error_without_optional_calls() {
        let mut request = request(&[".A", ".B"]);
        request.include_choice = true;
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut backend = FakeBackend {
            values: VecDeque::from([Err("invalid address".into())]),
            clock: Some(clock.clone()),
            advance_evaluation: Duration::from_secs(5),
            ..Default::default()
        };
        let response =
            read_registers_with_clock(&request, &mut backend, &mut || clock.get()).unwrap();

        assert_eq!(backend.calls, ["state", "PER.ADDRESS(\".A\")"]);
        assert_eq!(response["stop_reason"]["code"], "DEADLINE_EXCEEDED");
        assert_eq!(response["results"][0]["status"], "error");
        assert_eq!(
            response["results"][0]["address"]["error"]["code"],
            "NATIVE_EVALUATION_FAILED"
        );
        for field in ["value", "choice", "state_after"] {
            assert_eq!(
                response["results"][0][field]["reason_code"],
                "DEADLINE_EXCEEDED"
            );
        }
        assert_eq!(response["results"][1]["status"], "not_attempted");
        assert!(response["results"][1]["started_unix_ms"].is_null());
    }

    #[test]
    fn slow_final_post_state_marks_batch_deadline_exceeded() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut backend = FakeBackend {
            clock: Some(clock.clone()),
            advance_state: Duration::from_millis(2000),
            ..Default::default()
        };
        let result =
            read_registers_with_clock(&request(&[".A", ".B"]), &mut backend, &mut || clock.get())
                .unwrap();
        assert_eq!(result["results"][0]["value"]["ok"], true);
        assert_eq!(result["results"][0]["state_stable"], true);
        assert_eq!(result["results"][1]["status"], "not_attempted");
        assert_eq!(result["status"], "stopped");
        assert_eq!(result["deadline_exceeded"], true);
        assert_eq!(result["counts"]["evaluations"], 2);
    }

    #[test]
    fn oversized_native_value_is_not_silently_truncated_or_reinterpreted() {
        let mut backend = FakeBackend {
            values: VecDeque::from([evaluation(&"X".repeat(MAX_VALUE_BYTES + 1), 4)]),
            ..Default::default()
        };
        let result = run(&request(&[".A", ".B"]), &mut backend).unwrap();
        assert_eq!(
            result["results"][0]["address"]["error"]["code"],
            "VALUE_TOO_LARGE"
        );
        assert_eq!(result["results"][0]["address"]["value"], Value::Null);
        assert_eq!(
            result["results"][0]["value"]["reason_code"],
            "ADDRESS_FAILED"
        );
        assert_eq!(result["results"][1]["status"], "ok");
    }

    #[test]
    fn total_value_budget_stops_additional_expressions_and_retains_prior_values() {
        let mut request = request(&[".A"]);
        request.paths = (0..MAX_PATHS).map(|i| format!(".R{i}")).collect();
        let mut backend = FakeBackend {
            values: (0..MAX_PATHS * 2)
                .map(|_| evaluation(&"X".repeat(MAX_VALUE_BYTES), 4))
                .collect(),
            ..Default::default()
        };
        let result = run(&request, &mut backend).unwrap();
        assert_eq!(result["stop_reason"]["code"], "OUTPUT_LIMIT_REACHED");
        assert_eq!(result["counts"]["succeeded"], 8);
        assert_eq!(result["counts"]["evaluations"], 17);
        assert_eq!(result["counts"]["failed"], 1);
        assert_eq!(result["counts"]["not_attempted"], MAX_PATHS - 9);
        assert_eq!(
            result["results"][0]["address"]["value"]
                .as_str()
                .unwrap()
                .len(),
            MAX_VALUE_BYTES
        );
        assert!(serde_json::to_vec(&result).unwrap().len() < MAX_RESPONSE_BYTES);
    }

    #[test]
    fn native_errors_are_bounded_at_utf8_boundaries() {
        let mut backend = FakeBackend {
            values: VecDeque::from([Err("\u{1f4a5}".repeat(MAX_ERROR_BYTES))]),
            ..Default::default()
        };
        let result = run(&request(&[".A"]), &mut backend).unwrap();
        let error = &result["results"][0]["address"]["error"];
        assert_eq!(error["message_truncated"], true);
        let message = error["message"].as_str().unwrap();
        assert!(message.len() <= MAX_ERROR_BYTES);
        assert!(message.ends_with("... [truncated]"));
    }

    #[test]
    fn full_batch_and_escaped_error_output_stay_bounded() {
        let mut request = request(&[".A"]);
        request.paths = (0..MAX_PATHS)
            .map(|i| format!(".R{i}{}", "x".repeat(MAX_PATH_BYTES - 4)))
            .collect();
        let mut backend = FakeBackend {
            values: (0..MAX_PATHS)
                .map(|_| Err("\0".repeat(MAX_ERROR_BYTES)))
                .collect(),
            ..Default::default()
        };
        let result = run(&request, &mut backend).unwrap();
        assert_eq!(result["counts"]["failed"], MAX_PATHS);
        assert_eq!(result["results"].as_array().unwrap().len(), MAX_PATHS);
        assert!(serde_json::to_vec(&result).unwrap().len() < MAX_RESPONSE_BYTES);
    }

    #[test]
    fn timestamps_and_counts_are_present_for_attempted_and_skipped_items() {
        let mut backend = FakeBackend {
            states: VecDeque::from([Ok(2), Ok(2), Ok(3)]),
            ..Default::default()
        };
        let result = run(&request(&[".A", ".B"]), &mut backend).unwrap();
        for key in ["started_unix_ms", "completed_unix_ms", "elapsed_ms"] {
            assert!(result[key].is_u64());
            assert!(result["results"][0][key].is_u64());
        }
        assert!(result["results"][1]["started_unix_ms"].is_null());
        let counts = &result["counts"];
        assert_eq!(
            counts["succeeded"].as_u64().unwrap()
                + counts["partial"].as_u64().unwrap()
                + counts["failed"].as_u64().unwrap()
                + counts["not_attempted"].as_u64().unwrap(),
            counts["requested"].as_u64().unwrap()
        );
    }

    #[test]
    fn connection_failure_preserves_request_shape_without_native_calls() {
        let mut request = request(&[".A", ".B"]);
        request.include_choice = true;
        let result = connection_failure(&request, "No listener").unwrap();
        assert_eq!(result["status"], "connection_failed");
        assert_eq!(result["counts"]["not_attempted"], 2);
        assert_eq!(result["counts"]["evaluations"], 0);
        assert_eq!(result["counts"]["state_probes"], 0);
        assert_eq!(result["stop_reason"]["code"], "CONNECTION_FAILED");
        assert_eq!(result["stop_reason"]["message"], "No listener");
        assert_eq!(result["results"][0]["path"], ".A");
        assert_eq!(result["results"][1]["path"], ".B");
        assert_eq!(result["results"][0]["choice"]["status"], "not_attempted");
        assert_eq!(result["state_before"], Value::Null);
    }

    #[test]
    fn generated_schema_documents_risk_and_uses_strict_request_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(ReadPerRegistersRequest)).unwrap();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["include_value"]["default"], true);
        assert_eq!(
            schema["properties"]["acknowledge_side_effects"]["default"],
            false
        );
        assert!(
            schema["properties"]["acknowledge_side_effects"]["description"]
                .as_str()
                .unwrap()
                .contains("writes")
        );
    }
}
