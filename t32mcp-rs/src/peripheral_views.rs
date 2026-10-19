// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Bounded PER tree selections using the active default peripheral definition.
//! Callers must serialize native access. PER definitions may execute code,
//! including writes, or read side-effectful registers; these helpers are not a
//! read-only sandbox.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const MAX_VIEWS: usize = 16;
const MAX_COMPONENTS: usize = 16;
const MAX_COMPONENT_BYTES: usize = 128;
const MAX_WINDOW_BYTES: usize = 8192;
const MAX_BATCH_BYTES: usize = 65536;
const MAX_ERROR_BYTES: usize = 1024;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;

fn default_window_bytes() -> usize {
    4096
}

fn default_timeout_ms() -> u64 {
    5000
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PerViewSelection {
    /// PER tree path components, not a dotted PER.ADDRESS register path.
    /// At most 16 components; each is 1..128 ASCII bytes using letters,
    /// digits, spaces, underscore, dot, colon, hyphen, brackets or parentheses.
    /// An empty path selects the default PER view without root expansion.
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub subtree: Vec<String>,
    /// Append the controlled "*" expansion to a nonempty selected subtree.
    /// Expanding the entire root is deliberately unsupported.
    #[serde(default)]
    pub expand: bool,
    /// Native byte offset, not a line number.
    #[serde(default)]
    pub offset: u32,
    /// Native bytes to request, from 1 through 8192; at most 65536 per batch.
    #[serde(default = "default_window_bytes")]
    #[schemars(range(min = 1, max = 8192))]
    pub max_bytes: usize,
}

impl PerViewSelection {
    fn command(&self) -> String {
        if self.subtree.is_empty() {
            return "PER.view".into();
        }
        let mut path = self.subtree.join(",");
        if self.expand {
            path.push_str(",*");
        }
        // The comma omits the filename and selects the existing default PER.
        format!("PER.view , \"{path}\"")
    }

    fn validate(&self, index: usize) -> Result<(), String> {
        if self.subtree.len() > MAX_COMPONENTS {
            return Err(format!(
                "views[{index}].subtree must contain at most {MAX_COMPONENTS} components"
            ));
        }
        if self.subtree.is_empty() && self.expand {
            return Err(format!(
                "views[{index}].expand requires a nonempty subtree; root expansion is not allowed"
            ));
        }
        for (component_index, component) in self.subtree.iter().enumerate() {
            validate_component(component, index, component_index)?;
        }
        if !(1..=MAX_WINDOW_BYTES).contains(&self.max_bytes) {
            return Err(format!(
                "views[{index}].max_bytes must be between 1 and {MAX_WINDOW_BYTES}"
            ));
        }
        self.offset
            .checked_add(self.max_bytes as u32)
            .ok_or_else(|| {
                format!("views[{index}] offset plus max_bytes exceeds the 32-bit range")
            })?;
        Ok(())
    }
}

fn validate_component(component: &str, index: usize, component_index: usize) -> Result<(), String> {
    if component.is_empty() || component.len() > MAX_COMPONENT_BYTES || component.trim().is_empty()
    {
        return Err(format!(
            "views[{index}].subtree[{component_index}] must contain 1..{MAX_COMPONENT_BYTES} nonblank ASCII bytes"
        ));
    }
    if !component.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b' ' | b'_' | b'.' | b':' | b'-' | b'[' | b']' | b'(' | b')'
            )
    }) {
        return Err(format!(
            "views[{index}].subtree[{component_index}] contains unsupported characters; use ASCII letters, digits, spaces, _, ., :, -, [, ], (, )"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadPerViewsRequest {
    /// Between 1 and 16 bounded tree selections, queried in order.
    #[schemars(length(min = 1, max = 16))]
    pub views: Vec<PerViewSelection>,
    /// Must explicitly be true. PER definitions can execute code or access
    /// side-effectful registers even when the CPU is stopped.
    #[serde(default)]
    pub acknowledge_side_effects: bool,
    /// Soft deadline, checked between native calls; cannot interrupt blocked FFI.
    #[serde(default = "default_timeout_ms")]
    #[schemars(range(min = 1, max = 60000))]
    pub timeout_ms: u64,
}

impl ReadPerViewsRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !self.acknowledge_side_effects {
            return Err(
                "acknowledge_side_effects must be true: PER definitions can execute code or access side-effectful registers"
                    .into(),
            );
        }
        if !(1..=MAX_VIEWS).contains(&self.views.len()) {
            return Err(format!(
                "views must contain between 1 and {MAX_VIEWS} items"
            ));
        }
        if !(1..=60000).contains(&self.timeout_ms) {
            return Err("timeout_ms must be between 1 and 60000".into());
        }
        let mut total = 0usize;
        for (index, view) in self.views.iter().enumerate() {
            view.validate(index)?;
            total += view.max_bytes;
        }
        if total > MAX_BATCH_BYTES {
            return Err(format!(
                "total requested max_bytes must not exceed {MAX_BATCH_BYTES}"
            ));
        }
        Ok(())
    }
}

fn bounded_error(message: &str) -> String {
    if message.len() <= MAX_ERROR_BYTES {
        return message.into();
    }
    let mut end = MAX_ERROR_BYTES - " [truncated]".len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &message[..end])
}

fn stop_reason(code: &str, message: &str, index: usize, phase: &str) -> Value {
    json!({
        "code": code,
        "message": bounded_error(message),
        "view_index": index,
        "phase": phase,
    })
}

fn deadline_reason(index: usize, phase: &str) -> Value {
    stop_reason(
        "deadline_exceeded",
        "soft deadline expired; no further native calls were made",
        index,
        phase,
    )
}

fn result_base(index: usize, view: &PerViewSelection) -> Value {
    json!({
        "index": index,
        "query": view,
        "command": view.command(),
        "status": "not_attempted",
        "ok": false,
        "attempted": false,
        "state_verified": false,
        "text": null,
        "bytes_read": null,
        "next_offset": null,
        "may_have_more": null,
    })
}

fn not_attempted(index: usize, view: &PerViewSelection, reason: &Value) -> Value {
    let mut result = result_base(index, view);
    result["error"] = json!({
        "code": "not_attempted",
        "message": "Batch stopped before this selection was read",
        "stop_code": reason["code"],
    });
    result
}

#[derive(Default)]
struct StateGuard {
    initial: Option<i32>,
    last: Option<i32>,
    checks: usize,
}

impl StateGuard {
    fn check(
        &mut self,
        state: &mut impl FnMut() -> Result<i32, String>,
        index: usize,
        phase: &str,
    ) -> Result<(), Value> {
        self.checks += 1;
        let observed = match state() {
            Ok(observed) => observed,
            Err(error) => {
                self.last = None;
                return Err(stop_reason("state_unknown", &error, index, phase));
            }
        };
        self.last = Some(observed);
        let initial = *self.initial.get_or_insert(observed);
        let mut reason = if !matches!(observed, 1 | 2) {
            stop_reason(
                "target_not_stopped",
                "PER views require an already stopped target in state 1 or 2",
                index,
                phase,
            )
        } else if observed != initial {
            stop_reason(
                "state_changed",
                "target state changed during the PER view batch",
                index,
                phase,
            )
        } else {
            return Ok(());
        };
        reason["observed_state"] = json!(observed);
        reason["initial_state"] = json!(initial);
        Err(reason)
    }

    fn before_read(
        &mut self,
        state: &mut impl FnMut() -> Result<i32, String>,
        elapsed: &mut impl FnMut() -> Duration,
        deadline: Duration,
        index: usize,
    ) -> Result<(), Value> {
        if elapsed() >= deadline {
            return Err(deadline_reason(index, "before_state"));
        }
        self.check(state, index, "before_read")?;
        if elapsed() >= deadline {
            return Err(deadline_reason(index, "before_read"));
        }
        Ok(())
    }

    fn after_read(
        &mut self,
        result: &mut Value,
        state: &mut impl FnMut() -> Result<i32, String>,
        elapsed: &mut impl FnMut() -> Duration,
        deadline: Duration,
        index: usize,
    ) -> Option<Value> {
        // A timed-out native call cannot be interrupted. Preserve its result
        // as unverified rather than issuing another call beyond the deadline.
        if elapsed() >= deadline {
            let reason = deadline_reason(index, "after_read");
            self.last = None;
            mark_unverified(result, &reason);
            return Some(reason);
        }
        if let Err(reason) = self.check(state, index, "after_read") {
            mark_unverified(result, &reason);
            return Some(reason);
        }
        result["state_verified"] = json!(true);
        (elapsed() >= deadline).then(|| deadline_reason(index, "after_state"))
    }
}

fn finish(
    request: &ReadPerViewsRequest,
    results: Vec<Value>,
    state: StateGuard,
    stopped: Option<Value>,
    elapsed: Duration,
) -> Value {
    let succeeded = results.iter().filter(|item| item["ok"] == true).count();
    let attempted = results
        .iter()
        .filter(|item| item["attempted"] == true)
        .count();
    let failed = results
        .iter()
        .filter(|item| item["status"] == "error")
        .count();
    let status = if stopped.is_some() {
        "stopped"
    } else if failed > 0 {
        "completed_with_errors"
    } else {
        "completed"
    };
    json!({
        "ok": stopped.is_none() && failed == 0,
        "completed": stopped.is_none(),
        "status": status,
        "results": results,
        "summary": {
            "requested": request.views.len(),
            "attempted": attempted,
            "succeeded": succeeded,
            "failed": failed,
            "not_attempted": request.views.len() - attempted,
            "total_requested_bytes": request.views.iter().map(|view| view.max_bytes).sum::<usize>(),
        },
        "target_state": {
            "initial": state.initial,
            "last": state.last,
            "checks": state.checks,
        },
        "stop_reason": stopped,
        "acknowledge_side_effects": request.acknowledge_side_effects,
        "atomic": false,
        "limitations": [
            "Page byte limits bound returned text, not the MMIO accesses or PER interpreter statements needed to render a view.",
            "Each page can regenerate the selected view; repeated pages are not an atomic or stable snapshot.",
            "Stopped-state checks do not prevent PER definition side effects or concurrent target changes.",
            "The soft deadline cannot interrupt a blocked native call.",
        ],
        "soft_timeout": true,
        "timeout_ms": request.timeout_ms,
        "elapsed_ms": elapsed.as_millis(),
    })
}

fn page_result(
    index: usize,
    view: &PerViewSelection,
    page: Result<(String, usize), String>,
) -> Value {
    let mut result = result_base(index, view);
    result["attempted"] = json!(true);
    let failure = match page {
        Ok((text, bytes_read)) => {
            // Native lossy UTF-8 conversion expands at most 3x. Bound escaped
            // output too, including backend replacements used by offline tests.
            let valid = bytes_read <= view.max_bytes
                && text.len() <= bytes_read.saturating_mul(3)
                && serde_json::to_string(&text)
                    .is_ok_and(|encoded| encoded.len() <= bytes_read.saturating_mul(6) + 2);
            if valid {
                let may_have_more = bytes_read > 0;
                result["status"] = json!("ok");
                result["ok"] = json!(true);
                result["text"] = json!(text);
                result["bytes_read"] = json!(bytes_read);
                result["may_have_more"] = json!(may_have_more);
                result["next_offset"] = json!(if may_have_more {
                    Some(view.offset + bytes_read as u32)
                } else {
                    None
                });
                return result;
            }
            json!({
                "code": "invalid_window_response",
                "message": "window reader returned an invalid byte count or oversized text",
            })
        }
        Err(error) => json!({
            "code": "window_read_failed",
            "message": bounded_error(&error),
        }),
    };
    result["status"] = json!("error");
    result["error"] = failure;
    result
}

fn mark_unverified(result: &mut Value, reason: &Value) {
    result["ok"] = json!(false);
    result["status"] = json!("error");
    result["state_verified"] = json!(false);
    if result.get("error").is_none() {
        result["error"] = reason.clone();
    } else {
        result["guard_error"] = reason.clone();
    }
}

/// Read bounded selections on the caller's connected, serialized native thread.
/// This never changes conditions, Watch selections, or target execution state.
pub fn read_views(request: &ReadPerViewsRequest) -> Result<Value, String> {
    read_views_with_backend(request, crate::t32_state, |command, offset, max_bytes| {
        crate::t32_read_window(command, offset, max_bytes)
            .map(|content| (content.text, content.bytes_read))
    })
}

/// Injectable offline backend. All selections are validated before any call.
pub fn read_views_with_backend(
    request: &ReadPerViewsRequest,
    state: impl FnMut() -> Result<i32, String>,
    read: impl FnMut(&str, u32, usize) -> Result<(String, usize), String>,
) -> Result<Value, String> {
    let started = Instant::now();
    read_views_with_clock(request, state, read, || started.elapsed())
}

fn read_views_with_clock(
    request: &ReadPerViewsRequest,
    mut state: impl FnMut() -> Result<i32, String>,
    mut read: impl FnMut(&str, u32, usize) -> Result<(String, usize), String>,
    mut elapsed: impl FnMut() -> Duration,
) -> Result<Value, String> {
    request.validate()?;
    let deadline = Duration::from_millis(request.timeout_ms);
    let mut results = Vec::with_capacity(request.views.len());
    let mut guard = StateGuard::default();
    let mut stopped = None;
    for (index, view) in request.views.iter().enumerate() {
        if stopped.is_none() {
            stopped = guard
                .before_read(&mut state, &mut elapsed, deadline, index)
                .err();
        }
        if let Some(reason) = &stopped {
            results.push(not_attempted(index, view, reason));
            continue;
        }
        let mut result = page_result(
            index,
            view,
            read(&view.command(), view.offset, view.max_bytes),
        );
        stopped = guard.after_read(&mut result, &mut state, &mut elapsed, deadline, index);
        results.push(result);
    }
    let response = finish(request, results, guard, stopped, elapsed());
    if serde_json::to_vec(&response)
        .map_err(|error| format!("could not encode PER view response: {error}"))?
        .len()
        > MAX_OUTPUT_BYTES
    {
        return Err("PER view response exceeded the 512 KiB output limit".into());
    }
    Ok(response)
}

/// Describe a connection failure without claiming any window was attempted.
pub fn connection_failure(request: &ReadPerViewsRequest, error: &str) -> Value {
    unattempted_response(request, "connection_failed", error, "connect")
}

/// Refuse a user-disabled capability without connecting or rendering a view.
pub fn configured_unavailable(request: &ReadPerViewsRequest, error: &str) -> Value {
    unattempted_response(request, "configured_unsupported", error, "configuration")
}

fn unattempted_response(
    request: &ReadPerViewsRequest,
    code: &str,
    error: &str,
    stage: &str,
) -> Value {
    if let Err(error) = request.validate() {
        return json!({
            "ok": false,
            "completed": false,
            "status": "invalid_request",
            "error": {"code": "invalid_request", "message": bounded_error(&error)},
            "results": [],
        });
    }
    let reason = stop_reason(code, error, 0, stage);
    let results = request
        .views
        .iter()
        .enumerate()
        .map(|(index, view)| not_attempted(index, view, &reason))
        .collect();
    finish(
        request,
        results,
        StateGuard::default(),
        Some(reason),
        Duration::ZERO,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    fn request(views: Value) -> ReadPerViewsRequest {
        serde_json::from_value(json!({
            "views": views,
            "acknowledge_side_effects": true,
        }))
        .unwrap()
    }

    fn no_calls(request: &ReadPerViewsRequest) -> Result<Value, String> {
        read_views_with_backend(
            request,
            || panic!("state must not be called"),
            |_, _, _| panic!("window must not be called"),
        )
    }

    fn pages(request: &ReadPerViewsRequest, items: Vec<Result<(String, usize), String>>) -> Value {
        let mut items: VecDeque<_> = items.into();
        read_views_with_backend(
            request,
            || Ok(1),
            |_, _, _| items.pop_front().expect("unexpected read"),
        )
        .unwrap()
    }

    fn pages_with_states(
        request: &ReadPerViewsRequest,
        states: impl IntoIterator<Item = i32>,
    ) -> (Value, usize) {
        let mut states: VecDeque<_> = states.into_iter().collect();
        let mut reads = 0;
        let response = read_views_with_backend(
            request,
            || Ok(states.pop_front().expect("unexpected state check")),
            |_, _, _| {
                reads += 1;
                Ok(("a".into(), 1))
            },
        )
        .unwrap();
        (response, reads)
    }

    #[test]
    fn defaults_are_bounded_and_acknowledgement_is_not_implied() {
        let request: ReadPerViewsRequest = serde_json::from_value(json!({"views": [{}]})).unwrap();
        assert_eq!(request.timeout_ms, 5000);
        assert!(!request.acknowledge_side_effects);
        assert_eq!(request.views[0].max_bytes, 4096);
        assert_eq!(request.views[0].offset, 0);
        assert!(request.views[0].subtree.is_empty());
        assert!(!request.views[0].expand);
        assert!(no_calls(&request).unwrap_err().contains("acknowledge"));
    }

    #[test]
    fn unknown_fields_are_rejected_at_both_levels() {
        assert!(
            serde_json::from_value::<ReadPerViewsRequest>(json!({
                "views": [{}], "acknowledge_side_effects": true, "conditions": false,
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ReadPerViewsRequest>(json!({
                "views": [{"filename": "other.per"}], "acknowledge_side_effects": true,
            }))
            .is_err()
        );
    }

    #[test]
    fn validation_rejects_injection_before_any_native_call() {
        for component in [
            "",
            " ",
            "RCC,GPIO",
            "RCC\"",
            "RCC;Go",
            "RCC\\GPIO",
            "RCC/GPIO",
            "&macro",
            "%text",
            "*",
            "?",
            "RCC\n",
            "RCC\t",
            "RCC\0",
            "\u{00e9}",
        ] {
            let request = request(json!([{}, {"subtree": [component]}]));
            assert!(no_calls(&request).is_err(), "accepted {component:?}");
        }
        let request = request(json!([{"subtree": ["X".repeat(129)]}]));
        assert!(no_calls(&request).is_err());
    }

    #[test]
    fn validation_reports_invalid_component_before_window_limits() {
        let request = request(json!([
            {},
            {"subtree": ["RCC", "bad;Go"], "max_bytes": 0},
        ]));
        assert_eq!(
            no_calls(&request).unwrap_err(),
            "views[1].subtree[1] contains unsupported characters; use ASCII letters, digits, spaces, _, ., :, -, [, ], (, )"
        );
    }

    #[test]
    fn validated_commands_select_default_per_without_global_mutations() {
        let request = request(json!([
            {},
            {"subtree": ["Core Registers"]},
            {"subtree": ["RCC", "Bus_1.Status:0-1[2]"], "expand": true},
        ]));
        let mut commands = Vec::new();
        let response = read_views_with_backend(
            &request,
            || Ok(2),
            |command, _, _| {
                commands.push(command.to_string());
                Ok((String::new(), 0))
            },
        )
        .unwrap();
        assert_eq!(
            commands,
            [
                "PER.view",
                "PER.view , \"Core Registers\"",
                "PER.view , \"RCC,Bus_1.Status:0-1[2],*\"",
            ]
        );
        assert!(commands.iter().all(|command| {
            !command.contains("Watch")
                && !command.contains("CONDITIONS")
                && !command.contains("ReProgram")
                && !command.contains(';')
        }));
        assert_eq!(response["ok"], true);
        assert_eq!(response["target_state"]["checks"], 6);
        assert_eq!(response["atomic"], false);
    }

    #[test]
    fn parenthesized_vendor_titles_remain_one_quoted_component() {
        let request = request(json!([
            {"subtree": ["Memory Protection Unit (MPU)"], "expand": true},
        ]));
        let response = read_views_with_backend(
            &request,
            || Ok(1),
            |command, _, _| {
                assert_eq!(command, "PER.view , \"Memory Protection Unit (MPU),*\"");
                Ok((String::new(), 0))
            },
        )
        .unwrap();
        assert_eq!(response["ok"], true);
    }

    #[test]
    fn batch_component_and_deadline_limits_validate_before_calls() {
        assert!(no_calls(&request(json!([]))).is_err());
        assert!(no_calls(&request(json!(vec![json!({}); 17]))).is_err());
        assert!(
            no_calls(&request(json!([{"subtree": vec!["RCC"; 17]}])))
                .unwrap_err()
                .contains("components")
        );
        assert!(
            no_calls(&request(json!([{"expand": true}])))
                .unwrap_err()
                .contains("root expansion")
        );
        let mut request = request(json!([{}]));
        for timeout in [0, 60001, u64::MAX] {
            request.timeout_ms = timeout;
            assert!(no_calls(&request).unwrap_err().contains("timeout_ms"));
        }
        request.timeout_ms = 60000;
        request.views[0].subtree = vec!["X".repeat(128); 16];
        assert!(request.validate().is_ok());
    }

    #[test]
    fn byte_and_offset_limits_validate_before_calls() {
        for max_bytes in [0, 8193, usize::MAX] {
            assert!(no_calls(&request(json!([{"max_bytes": max_bytes}]))).is_err());
        }
        assert!(
            no_calls(&request(json!([{"offset": u32::MAX, "max_bytes": 1}])))
                .unwrap_err()
                .contains("32-bit")
        );
        assert!(
            no_calls(&request(json!(vec![json!({"max_bytes": 8192}); 9])))
                .unwrap_err()
                .contains("total")
        );
        assert!(
            request(json!([{"offset": u32::MAX - 1, "max_bytes": 1}]))
                .validate()
                .is_ok()
        );
        assert!(
            request(json!(vec![json!({"max_bytes": 8192}); 8]))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn one_window_failure_does_not_hide_other_selections() {
        let response = pages(
            &request(json!([{}, {}, {}])),
            vec![
                Ok(("a".into(), 1)),
                Err("native window unavailable".into()),
                Ok(("c".into(), 1)),
            ],
        );
        assert_eq!(response["status"], "completed_with_errors");
        assert_eq!(response["completed"], true);
        assert_eq!(response["ok"], false);
        assert_eq!(response["summary"]["succeeded"], 2);
        assert_eq!(response["summary"]["failed"], 1);
        assert_eq!(response["summary"]["not_attempted"], 0);
        assert_eq!(response["results"][0]["text"], "a");
        assert_eq!(response["results"][2]["text"], "c");
        assert_eq!(
            response["results"][1]["error"]["code"],
            "window_read_failed"
        );
        assert_eq!(response["results"][1]["state_verified"], true);
        assert!(response["results"][1]["may_have_more"].is_null());
        assert!(response["stop_reason"].is_null());
    }

    #[test]
    fn short_pages_continue_and_only_zero_bytes_establish_eof() {
        let response = pages(
            &request(json!([{"offset": 10}, {"offset": 11}])),
            vec![Ok(("a".into(), 1)), Ok(("".into(), 0))],
        );
        assert_eq!(response["results"][0]["may_have_more"], true);
        assert_eq!(response["results"][0]["next_offset"], 11);
        assert_eq!(response["results"][1]["may_have_more"], false);
        assert!(response["results"][1]["next_offset"].is_null());
        assert_eq!(response["results"][1]["bytes_read"], 0);
    }

    #[test]
    fn running_down_and_unknown_targets_are_not_read() {
        for state in [0, 3, 4, -1] {
            let response = read_views_with_backend(
                &request(json!([{}, {}])),
                || Ok(state),
                |_, _, _| panic!("unsafe target must not be read"),
            )
            .unwrap();
            assert_eq!(response["stop_reason"]["code"], "target_not_stopped");
            assert_eq!(response["summary"]["attempted"], 0);
            assert_eq!(response["summary"]["not_attempted"], 2);
            assert_eq!(response["target_state"]["checks"], 1);
        }
    }

    #[test]
    fn state_failure_before_read_stops_the_batch() {
        let response = read_views_with_backend(
            &request(json!([{}])),
            || Err("disconnected".into()),
            |_, _, _| panic!("unknown state must not be read"),
        )
        .unwrap();
        assert_eq!(response["stop_reason"]["code"], "state_unknown");
        assert_eq!(response["results"][0]["status"], "not_attempted");
        assert!(response["target_state"]["last"].is_null());
    }

    #[test]
    fn state_change_between_views_preserves_finished_selection() {
        let (response, reads) = pages_with_states(&request(json!([{}, {}])), [1, 1, 2]);
        assert_eq!(reads, 1);
        assert_eq!(response["stop_reason"]["code"], "state_changed");
        assert_eq!(response["results"][0]["ok"], true);
        assert_eq!(response["results"][1]["status"], "not_attempted");
    }

    #[test]
    fn post_read_state_change_marks_data_unverified_and_stops() {
        let (response, reads) = pages_with_states(&request(json!([{}, {}])), [1, 3]);
        assert_eq!(reads, 1);
        assert_eq!(response["stop_reason"]["code"], "target_not_stopped");
        assert_eq!(response["results"][0]["text"], "a");
        assert_eq!(response["results"][0]["ok"], false);
        assert_eq!(response["results"][0]["state_verified"], false);
        assert_eq!(response["results"][1]["status"], "not_attempted");
    }

    #[test]
    fn post_read_state_failure_preserves_independent_window_error() {
        let mut states = VecDeque::from([Ok(1), Err("state disconnected".into())]);
        let response = read_views_with_backend(
            &request(json!([{}, {}])),
            || states.pop_front().unwrap(),
            |_, _, _| Err("window failed".into()),
        )
        .unwrap();
        assert_eq!(
            response["results"][0]["error"]["code"],
            "window_read_failed"
        );
        assert_eq!(
            response["results"][0]["guard_error"]["code"],
            "state_unknown"
        );
        assert_eq!(response["results"][1]["status"], "not_attempted");
    }

    #[test]
    fn invalid_backend_pages_are_bounded_independent_errors() {
        for page in [
            ("abcdef".into(), 6),
            ("abcdef".into(), 1),
            ("not EOF".into(), 0),
            ("\0\0".into(), 1),
        ] {
            let response = pages(&request(json!([{"max_bytes": 4}])), vec![Ok(page)]);
            assert_eq!(
                response["results"][0]["error"]["code"],
                "invalid_window_response"
            );
            assert!(response["results"][0]["text"].is_null());
            assert!(response["results"][0]["may_have_more"].is_null());
        }
    }

    #[test]
    fn native_lossy_text_preserves_native_byte_pagination() {
        let response = pages(
            &request(json!([{"offset": 10, "max_bytes": 1}])),
            vec![Ok(("\u{fffd}".into(), 1))],
        );
        assert_eq!(response["results"][0]["ok"], true);
        assert_eq!(response["results"][0]["bytes_read"], 1);
        assert_eq!(response["results"][0]["next_offset"], 11);
    }

    #[test]
    fn deadline_before_first_call_has_no_native_calls() {
        let response = read_views_with_clock(
            &request(json!([{}, {}])),
            || panic!("deadline must prevent state call"),
            |_, _, _| panic!("deadline must prevent window call"),
            || Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(response["stop_reason"]["code"], "deadline_exceeded");
        assert_eq!(response["summary"]["attempted"], 0);
        assert_eq!(response["target_state"]["checks"], 0);
    }

    #[test]
    fn deadline_during_pre_state_prevents_window_call() {
        let elapsed = Cell::new(Duration::ZERO);
        let response = read_views_with_clock(
            &request(json!([{}])),
            || {
                elapsed.set(Duration::from_secs(5));
                Ok(1)
            },
            |_, _, _| panic!("deadline must prevent window call"),
            || elapsed.get(),
        )
        .unwrap();
        assert_eq!(response["stop_reason"]["phase"], "before_read");
        assert_eq!(response["summary"]["attempted"], 0);
        assert_eq!(response["target_state"]["checks"], 1);
    }

    #[test]
    fn failed_pre_state_at_deadline_preserves_state_failure() {
        let elapsed = Cell::new(Duration::ZERO);
        let response = read_views_with_clock(
            &request(json!([{}, {}])),
            || {
                elapsed.set(Duration::from_secs(5));
                Err("state disconnected".into())
            },
            |_, _, _| panic!("unknown state must prevent window call"),
            || elapsed.get(),
        )
        .unwrap();
        assert_eq!(response["stop_reason"]["code"], "state_unknown");
        assert_eq!(response["stop_reason"]["phase"], "before_read");
        assert_eq!(response["summary"]["not_attempted"], 2);
        assert_eq!(response["target_state"]["checks"], 1);
    }

    #[test]
    fn deadline_during_read_prevents_post_state_and_further_reads() {
        let elapsed = Cell::new(Duration::ZERO);
        let states = Cell::new(0);
        let response = read_views_with_clock(
            &request(json!([{}, {}])),
            || {
                states.set(states.get() + 1);
                Ok(1)
            },
            |_, _, _| {
                elapsed.set(Duration::from_secs(5));
                Ok(("a".into(), 1))
            },
            || elapsed.get(),
        )
        .unwrap();
        assert_eq!(states.get(), 1);
        assert_eq!(response["stop_reason"]["phase"], "after_read");
        assert_eq!(response["results"][0]["text"], "a");
        assert_eq!(response["results"][0]["state_verified"], false);
        assert_eq!(response["results"][0]["ok"], false);
        assert_eq!(response["results"][1]["status"], "not_attempted");
        assert!(response["target_state"]["last"].is_null());
    }

    #[test]
    fn failed_read_at_deadline_preserves_window_error_and_skips_post_state() {
        let elapsed = Cell::new(Duration::ZERO);
        let states = Cell::new(0);
        let response = read_views_with_clock(
            &request(json!([{}, {}])),
            || {
                states.set(states.get() + 1);
                Ok(1)
            },
            |_, _, _| {
                elapsed.set(Duration::from_secs(5));
                Err("window failed".into())
            },
            || elapsed.get(),
        )
        .unwrap();
        assert_eq!(states.get(), 1);
        assert_eq!(
            response["results"][0]["error"]["code"],
            "window_read_failed"
        );
        assert_eq!(
            response["results"][0]["guard_error"]["code"],
            "deadline_exceeded"
        );
        assert_eq!(response["stop_reason"]["phase"], "after_read");
        assert_eq!(response["results"][0]["state_verified"], false);
        assert_eq!(response["results"][1]["status"], "not_attempted");
        assert!(response["target_state"]["last"].is_null());
    }

    #[test]
    fn deadline_during_post_state_preserves_verified_page_without_more_calls() {
        let elapsed = Cell::new(Duration::ZERO);
        let states = Cell::new(0);
        let reads = Cell::new(0);
        let response = read_views_with_clock(
            &request(json!([{}, {}])),
            || {
                states.set(states.get() + 1);
                if states.get() == 2 {
                    elapsed.set(Duration::from_secs(5));
                }
                Ok(1)
            },
            |_, _, _| {
                reads.set(reads.get() + 1);
                Ok(("a".into(), 1))
            },
            || elapsed.get(),
        )
        .unwrap();
        assert_eq!(states.get(), 2);
        assert_eq!(reads.get(), 1);
        assert_eq!(response["stop_reason"]["phase"], "after_state");
        assert_eq!(response["results"][0]["ok"], true);
        assert_eq!(response["results"][0]["state_verified"], true);
        assert_eq!(response["results"][1]["status"], "not_attempted");
    }

    #[test]
    fn large_error_messages_are_utf8_safe_and_bounded() {
        let response = pages(&request(json!([{}])), vec![Err("\u{00e9}".repeat(4096))]);
        let error = response["results"][0]["error"]["message"].as_str().unwrap();
        assert!(error.len() <= MAX_ERROR_BYTES);
        assert!(error.ends_with(" [truncated]"));
    }

    #[test]
    fn maximum_batch_and_path_metadata_stay_inside_output_cap() {
        let request = request(json!(vec![
            json!({
                "subtree": vec!["X".repeat(128); 16],
                "expand": true,
                "max_bytes": 4096,
            });
            16
        ]));
        let response = pages(&request, vec![Ok(("\0".repeat(4096), 4096)); 16]);
        assert_eq!(response["ok"], true);
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_OUTPUT_BYTES);
    }

    #[test]
    fn connection_failure_marks_all_valid_views_unattempted() {
        let response = connection_failure(&request(json!([{}, {}])), "TCP connection failed");
        assert_eq!(response["stop_reason"]["code"], "connection_failed");
        assert_eq!(response["summary"]["attempted"], 0);
        assert_eq!(response["summary"]["not_attempted"], 2);
        assert_eq!(response["target_state"]["checks"], 0);
        assert!(response["results"][0]["may_have_more"].is_null());
        assert!(response["results"][1]["bytes_read"].is_null());
        assert_eq!(response["elapsed_ms"], 0);
    }

    #[test]
    fn connection_failure_does_not_expand_invalid_requests() {
        let response = connection_failure(
            &request(json!(vec![json!({"subtree": ["X".repeat(4096)]}); 100])),
            &"x".repeat(100000),
        );
        assert_eq!(response["status"], "invalid_request");
        assert_eq!(response["results"], json!([]));
        assert!(serde_json::to_vec(&response).unwrap().len() < 2048);
    }
}
