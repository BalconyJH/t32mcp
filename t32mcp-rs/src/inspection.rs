// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Bounded inspection helpers. Callers must serialize access to the native API.
//! These helpers do not issue writes, Go, or Break, but reading MMIO or evaluating
//! target-specific functions can have side effects. They never connect implicitly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fmt::Write;
use tokio::time::{Duration, Instant, sleep};

const MAX_BATCH_ITEMS: usize = 64;
const MAX_TEXT_BYTES: usize = 4096;
const MAX_REGION_BYTES: usize = 4096;
const MAX_TOTAL_MEMORY_BYTES: usize = 65536;
const MAX_WINDOW_BYTES: usize = 8192;
const MAX_MISMATCH_OFFSETS: usize = 64;
const MAX_WAIT_SAMPLES: usize = 1000;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvaluateRequest {
    /// Expressions evaluated in order, at most 64. Results retain native types.
    pub expressions: Vec<String>,
}

impl EvaluateRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_batch(&self.expressions, "expressions")
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistersRequest {
    /// Register names, at most 64.
    pub names: Vec<String>,
}

impl RegistersRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_batch(&self.names, "register names")
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SymbolsRequest {
    /// Symbol names, at most 64.
    pub names: Vec<String>,
}

impl SymbolsRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_batch(&self.names, "symbol names")
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAccess {
    #[default]
    Data,
    Program,
    ArmDap,
}

impl MemoryAccess {
    pub(crate) fn native(self) -> i32 {
        match self {
            Self::Data => crate::ffi::T32_MEMORY_ACCESS_DATA as i32,
            Self::Program => crate::ffi::T32_MEMORY_ACCESS_PROGRAM as i32,
            Self::ArmDap => crate::ffi::T32_MEMORY_ACCESS_ARM_DAP as i32,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryRegion {
    /// Unsigned 32-bit address: decimal digits or hexadecimal prefixed with 0x.
    pub address: String,
    /// Bytes to read, from 1 through 4096. The address range must not wrap.
    pub length: usize,
    /// Access class supported by the current target/session. ARM DAP is opt-in.
    #[serde(default)]
    pub access: MemoryAccess,
    /// Request E-access only on supported targets. This server's running-target
    /// exception requires explicit arm_dap plus this flag; it is not portable.
    #[serde(default)]
    pub dual_port: bool,
    /// Optional expected bytes: contiguous or whitespace-separated hex, with an
    /// optional leading 0x. Must describe exactly `length` bytes.
    pub expected_hex: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryRequest {
    /// At most 64 regions and 65536 bytes in total. MMIO reads can have effects.
    pub regions: Vec<MemoryRegion>,
}

impl MemoryRequest {
    pub fn validate(&self) -> Result<(), String> {
        prepare_memory(self).map(|_| ())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    Register,
    Frame,
    VarLocal,
    BreakList,
    TaskList,
}

impl Window {
    fn command(self) -> &'static str {
        match self {
            Self::Register => "Register.view",
            Self::Frame => "Frame.view",
            Self::VarLocal => "Var.Local",
            Self::BreakList => "Break.List",
            Self::TaskList => "TASK.List",
        }
    }
}

fn default_window_bytes() -> usize {
    4096
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowRequest {
    pub window: Window,
    /// Native byte offset, not a line number.
    #[serde(default)]
    pub offset: u32,
    /// Maximum native bytes to read, from 1 through 8192.
    #[serde(default = "default_window_bytes")]
    pub max_bytes: usize,
}

impl WindowRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_WINDOW_BYTES).contains(&self.max_bytes) {
            return Err(format!(
                "max_bytes must be between 1 and {MAX_WINDOW_BYTES}"
            ));
        }
        self.offset
            .checked_add(self.max_bytes as u32)
            .ok_or_else(|| "window offset plus max_bytes exceeds the 32-bit range".to_string())?;
        Ok(())
    }
}

fn default_timeout_ms() -> u64 {
    5000
}

fn default_interval_ms() -> u64 {
    100
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    /// A single expression. No control characters or command separators.
    pub expression: String,
    /// Exact native result text to match, including numeric suffixes such as ".".
    pub expected_value: String,
    /// Polling deadline, from 1 through 60000 ms. Checked between native calls;
    /// this does not impose a timeout on a blocked native call.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Delay between samples, from 20 through 60000 ms.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

impl WaitRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_text(&self.expression, "expression")?;
        if self.expected_value.len() > MAX_TEXT_BYTES {
            return Err(format!(
                "expected_value must not exceed {MAX_TEXT_BYTES} bytes"
            ));
        }
        if !(1..=60000).contains(&self.timeout_ms) {
            return Err("timeout_ms must be between 1 and 60000".to_string());
        }
        if !(20..=60000).contains(&self.interval_ms) {
            return Err("interval_ms must be between 20 and 60000".to_string());
        }
        Ok(())
    }
}

fn validate_text(text: &str, label: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!("{label} must not exceed {MAX_TEXT_BYTES} bytes"));
    }
    if text.chars().any(char::is_control) || text.contains(';') {
        return Err(format!(
            "{label} must not contain control characters or semicolons"
        ));
    }
    Ok(())
}

fn validate_batch(items: &[String], label: &str) -> Result<(), String> {
    if items.is_empty() || items.len() > MAX_BATCH_ITEMS {
        return Err(format!(
            "{label} must contain between 1 and {MAX_BATCH_ITEMS} items"
        ));
    }
    for (index, text) in items.iter().enumerate() {
        validate_text(text, &format!("{label}[{index}]"))?;
    }
    Ok(())
}

fn observation<T: Serialize>(result: Result<T, String>) -> Value {
    match result {
        Ok(value) => json!({"ok": true, "value": value}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

fn batch_response(results: Vec<Value>) -> Value {
    let succeeded = results.iter().filter(|item| item["ok"] == true).count();
    json!({
        "count": results.len(),
        "succeeded": succeeded,
        "failed": results.len() - succeeded,
        "results": results,
    })
}

/// Each query is independent; unavailable optional status fields retain errors.
pub fn status(port: u16) -> Value {
    json!({
        "port": port,
        "state": observation(crate::t32_state()),
        "practice_state": observation(crate::t32_practice_state()),
        "software_version": observation(crate::t32_fnc("VERSION.SOFTWARE()".to_string())),
        "build": observation(crate::t32_fnc("VERSION.BUILD()".to_string())),
        "cpu": observation(crate::t32_fnc("SYStem.CPU()".to_string())),
    })
}

pub fn evaluate(request: EvaluateRequest) -> Result<Value, String> {
    evaluate_with(request, |expression| {
        crate::t32_eval(expression).map(|result| (result.value, result.result_type))
    })
}

fn evaluate_with(
    request: EvaluateRequest,
    mut evaluate: impl FnMut(&str) -> Result<(String, u32), String>,
) -> Result<Value, String> {
    request.validate()?;
    let results = request
        .expressions
        .iter()
        .map(|expression| match evaluate(expression) {
            Ok((value, result_type)) => json!({
                "expression": expression,
                "ok": true,
                "value": value,
                "result_type": result_type,
            }),
            Err(error) => json!({"expression": expression, "ok": false, "error": error}),
        })
        .collect();
    Ok(batch_response(results))
}

pub fn registers(names: Vec<String>) -> Result<Value, String> {
    registers_with(names, crate::t32_read_register)
}

fn registers_with(
    names: Vec<String>,
    mut read: impl FnMut(&str) -> Result<u64, String>,
) -> Result<Value, String> {
    validate_batch(&names, "register names")?;
    let results = names
        .iter()
        .map(|name| match read(name) {
            Ok(value) => json!({
                "name": name,
                "ok": true,
                "value": value,
                "hex": format!("0x{value:016X}"),
            }),
            Err(error) => json!({"name": name, "ok": false, "error": error}),
        })
        .collect();
    Ok(batch_response(results))
}

pub fn symbols(names: Vec<String>) -> Result<Value, String> {
    validate_batch(&names, "symbol names")?;
    let results = names
        .iter()
        .map(|name| match crate::t32_lookup_symbol(name) {
            Ok(symbol) => json!({
                "name": name,
                "ok": true,
                "address": symbol.address,
                "address_hex": format!("0x{:08X}", symbol.address),
                "size": symbol.size,
                "access": symbol.access,
            }),
            Err(error) => json!({"name": name, "ok": false, "error": error}),
        })
        .collect();
    Ok(batch_response(results))
}

struct PreparedRegion {
    address: u32,
    expected: Option<Vec<u8>>,
}

fn parse_address(address: &str) -> Result<u32, String> {
    if address.len() > MAX_TEXT_BYTES {
        return Err(format!("address must not exceed {MAX_TEXT_BYTES} bytes"));
    }
    let address = address.trim();
    let (digits, radix) = if let Some(digits) = address
        .strip_prefix("0x")
        .or_else(|| address.strip_prefix("0X"))
    {
        (digits, 16)
    } else {
        (address, 10)
    };
    if digits.is_empty()
        || !digits.bytes().all(|byte| {
            if radix == 16 {
                byte.is_ascii_hexdigit()
            } else {
                byte.is_ascii_digit()
            }
        })
    {
        return Err("address must be decimal digits or 0x-prefixed hexadecimal".to_string());
    }
    u32::from_str_radix(digits, radix)
        .map_err(|_| "address exceeds the unsigned 32-bit range".to_string())
}

fn parse_expected_hex(text: &str, length: usize) -> Result<Vec<u8>, String> {
    // Limit whitespace-heavy input as well as the decoded byte count.
    if text.len() > MAX_REGION_BYTES * 4 + 2 {
        return Err("expected_hex input is too long".to_string());
    }
    let text = text.trim();
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if digits.len() != length * 2 {
        return Err(format!("expected_hex must describe exactly {length} bytes"));
    }
    if !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err("expected_hex contains a non-hexadecimal character".to_string());
    }
    Ok(digits
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).expect("validated hex digit");
            let low = (pair[1] as char).to_digit(16).expect("validated hex digit");
            ((high << 4) | low) as u8
        })
        .collect())
}

fn prepare_memory(request: &MemoryRequest) -> Result<Vec<PreparedRegion>, String> {
    if request.regions.is_empty() || request.regions.len() > MAX_BATCH_ITEMS {
        return Err(format!(
            "regions must contain between 1 and {MAX_BATCH_ITEMS} items"
        ));
    }
    let mut total = 0usize;
    request
        .regions
        .iter()
        .enumerate()
        .map(|(index, region)| {
            let prepare = || -> Result<PreparedRegion, String> {
                if !(1..=MAX_REGION_BYTES).contains(&region.length) {
                    return Err(format!("length must be between 1 and {MAX_REGION_BYTES}"));
                }
                let address = parse_address(&region.address)?;
                if u64::from(address) + region.length as u64 > u64::from(u32::MAX) + 1 {
                    return Err(
                        "memory address range exceeds the unsigned 32-bit range".to_string()
                    );
                }
                let expected = region
                    .expected_hex
                    .as_deref()
                    .map(|text| parse_expected_hex(text, region.length))
                    .transpose()?;
                Ok(PreparedRegion { address, expected })
            };
            let prepared = prepare().map_err(|error| format!("regions[{index}]: {error}"))?;
            total += region.length;
            if total > MAX_TOTAL_MEMORY_BYTES {
                return Err(format!(
                    "total memory length must not exceed {MAX_TOTAL_MEMORY_BYTES} bytes"
                ));
            }
            Ok(prepared)
        })
        .collect()
}

fn hex_string(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut text, "{byte:02X}").expect("writing to String cannot fail");
    }
    text
}

fn hexdump(address: u32, bytes: &[u8]) -> String {
    let mut text = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        write!(
            &mut text,
            "{:08X}  ",
            u64::from(address) + (row * 16) as u64
        )
        .expect("writing to String cannot fail");
        let (left, right) = chunk.split_at(chunk.len().min(8));
        write_hex_columns(&mut text, left);
        text.push(' ');
        write_hex_columns(&mut text, right);
        text.push_str(" |");
        for byte in chunk {
            text.push(if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            });
        }
        text.push_str("|\n");
    }
    text
}

fn write_hex_columns(text: &mut String, bytes: &[u8]) {
    for byte in bytes {
        write!(text, "{byte:02X} ").expect("writing to String cannot fail");
    }
    text.push_str(&"   ".repeat(8 - bytes.len()));
}

pub fn memory(request: MemoryRequest) -> Result<Value, String> {
    memory_with(request, crate::t32_read_memory)
}

fn memory_with(
    request: MemoryRequest,
    mut read: impl FnMut(u32, usize, i32, bool) -> Result<Vec<u8>, String>,
) -> Result<Value, String> {
    // Finish validating every region before the first native read.
    let prepared = prepare_memory(&request)?;
    let results = request
        .regions
        .iter()
        .zip(prepared)
        .map(|(region, prepared)| read_memory_region(region, prepared, &mut read))
        .collect();
    let mut result = batch_response(results);
    result["total_requested_bytes"] = json!(
        request
            .regions
            .iter()
            .map(|region| region.length)
            .sum::<usize>()
    );
    Ok(result)
}

fn read_memory_region(
    region: &MemoryRegion,
    prepared: PreparedRegion,
    read: &mut impl FnMut(u32, usize, i32, bool) -> Result<Vec<u8>, String>,
) -> Value {
    let mut result = json!({
        "address": format!("0x{:08X}", prepared.address),
        "length": region.length,
        "access": region.access,
        "dual_port": region.dual_port,
    });
    let bytes = match read(
        prepared.address,
        region.length,
        region.access.native(),
        region.dual_port,
    ) {
        Ok(bytes) if bytes.len() == region.length => bytes,
        Ok(bytes) => {
            result["ok"] = json!(false);
            result["bytes_read"] = json!(bytes.len());
            result["error"] = json!(format!(
                "memory read returned {} bytes; expected {}",
                bytes.len(),
                region.length
            ));
            return result;
        }
        Err(error) => {
            result["ok"] = json!(false);
            result["error"] = json!(error);
            return result;
        }
    };
    result["ok"] = json!(true);
    result["bytes_read"] = json!(bytes.len());
    result["hex"] = json!(hex_string(&bytes));
    result["hexdump"] = json!(hexdump(prepared.address, &bytes));
    if let Some(expected) = prepared.expected {
        add_memory_comparison(&mut result, &bytes, &expected);
    }
    result
}

fn add_memory_comparison(result: &mut Value, bytes: &[u8], expected: &[u8]) {
    let mut mismatches = Vec::new();
    let mut mismatch_count = 0usize;
    for (offset, (actual, expected)) in bytes.iter().zip(expected).enumerate() {
        if actual != expected {
            mismatch_count += 1;
            if mismatches.len() < MAX_MISMATCH_OFFSETS {
                mismatches.push(offset);
            }
        }
    }
    result["expected_hex"] = json!(hex_string(expected));
    result["matches"] = json!(mismatch_count == 0);
    result["mismatch_count"] = json!(mismatch_count);
    result["mismatch_offsets"] = json!(mismatches);
    result["mismatch_offsets_truncated"] = json!(mismatch_count > MAX_MISMATCH_OFFSETS);
}

pub fn window(request: WindowRequest) -> Result<Value, String> {
    window_with(request, |command, offset, length| {
        crate::t32_read_window(command, offset, length)
            .map(|content| (content.text, content.bytes_read))
    })
}

fn window_with(
    request: WindowRequest,
    mut read: impl FnMut(&str, u32, usize) -> Result<(String, usize), String>,
) -> Result<Value, String> {
    request.validate()?;
    let (text, bytes_read) = read(request.window.command(), request.offset, request.max_bytes)?;
    if bytes_read > request.max_bytes {
        return Err("window reader returned more bytes than requested".to_string());
    }
    // The native API indicates EOF with zero bytes, not a short page.
    let may_have_more = bytes_read > 0;
    Ok(json!({
        "window": request.window,
        "command": request.window.command(),
        "offset": request.offset,
        "bytes_read": bytes_read,
        "max_bytes": request.max_bytes,
        "may_have_more": may_have_more,
        "next_offset": if may_have_more {
            Some(request.offset + bytes_read as u32)
        } else {
            None
        },
        "text": text,
    }))
}

pub async fn wait(request: WaitRequest) -> Result<Value, String> {
    wait_with(request, |expression| {
        crate::t32_eval(expression).map(|result| (result.value, result.result_type))
    })
    .await
}

fn retain_sample(samples: &mut VecDeque<Value>, sample: Value) {
    if samples.len() == MAX_WAIT_SAMPLES {
        samples.pop_front();
    }
    samples.push_back(sample);
}

async fn wait_with(
    request: WaitRequest,
    mut evaluate: impl FnMut(&str) -> Result<(String, u32), String>,
) -> Result<Value, String> {
    request.validate()?;
    let started = Instant::now();
    let timeout = Duration::from_millis(request.timeout_ms);
    let interval = Duration::from_millis(request.interval_ms);
    let mut samples = VecDeque::new();
    let mut attempts = 0usize;
    let mut matched = false;
    loop {
        // The synchronous native call can outlive this polling deadline.
        if attempts > 0 && started.elapsed() >= timeout {
            break;
        }
        attempts += 1;
        let evaluation = evaluate(&request.expression);
        let mut sample = json!({
            "attempt": attempts,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        });
        match evaluation {
            Ok((value, result_type)) => {
                matched = value == request.expected_value;
                sample["ok"] = json!(true);
                sample["value"] = json!(value);
                sample["result_type"] = json!(result_type);
                sample["matched"] = json!(matched);
            }
            Err(error) => {
                sample["ok"] = json!(false);
                sample["error"] = json!(error);
            }
        }
        retain_sample(&mut samples, sample);
        if matched {
            break;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        sleep(interval.min(remaining)).await;
    }
    let elapsed = started.elapsed();
    Ok(json!({
        "expression": request.expression,
        "expected_value": request.expected_value,
        "matched": matched,
        "timed_out": !matched && elapsed >= timeout,
        "attempts": attempts,
        "elapsed_ms": elapsed.as_millis() as u64,
        "timeout_ms": request.timeout_ms,
        "interval_ms": request.interval_ms,
        "timeout_scope": "between_native_calls",
        "deadline_exceeded": elapsed > timeout,
        "samples_dropped": attempts - samples.len(),
        "last_sample": samples.back(),
        "samples": samples,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(address: &str, length: usize, expected_hex: Option<&str>) -> MemoryRegion {
        MemoryRegion {
            address: address.to_string(),
            length,
            access: MemoryAccess::Data,
            dual_port: false,
            expected_hex: expected_hex.map(str::to_string),
        }
    }

    fn wait_request(expected: &str, timeout_ms: u64) -> WaitRequest {
        WaitRequest {
            expression: "Register(PC)".to_string(),
            expected_value: expected.to_string(),
            timeout_ms,
            interval_ms: 20,
        }
    }

    #[test]
    fn memory_defaults_do_not_select_target_specific_access_or_addresses() {
        let region: MemoryRegion =
            serde_json::from_value(json!({"address": "0x1000", "length": 4})).unwrap();
        assert!(matches!(region.access, MemoryAccess::Data));
        assert!(!region.dual_port);
        assert!(region.expected_hex.is_none());
        for incomplete in [json!({"length": 4}), json!({"address": "0x1000"})] {
            assert!(serde_json::from_value::<MemoryRegion>(incomplete).is_err());
        }
    }

    #[test]
    fn expression_validation_is_bounded_and_rejects_command_separators() {
        for expression in ["", " ", "1;Go", "1\nGo", "1\rGo", "1\0"] {
            assert!(
                EvaluateRequest {
                    expressions: vec![expression.to_string()],
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            EvaluateRequest {
                expressions: vec!["x".repeat(MAX_TEXT_BYTES + 1)],
            }
            .validate()
            .is_err()
        );
        assert!(
            EvaluateRequest {
                expressions: vec![]
            }
            .validate()
            .is_err()
        );
        assert!(
            EvaluateRequest {
                expressions: vec!["1".to_string(); MAX_BATCH_ITEMS + 1],
            }
            .validate()
            .is_err()
        );
        assert!(
            EvaluateRequest {
                expressions: vec!["Register(PC)".to_string(); MAX_BATCH_ITEMS],
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn evaluations_preserve_order_native_types_and_partial_errors() {
        let result = evaluate_with(
            EvaluateRequest {
                expressions: vec!["first".into(), "bad".into(), "last".into()],
            },
            |expression| {
                if expression == "bad" {
                    Err("unknown symbol".into())
                } else {
                    Ok((expression.to_string(), 8))
                }
            },
        )
        .unwrap();
        assert_eq!(result["succeeded"], 2);
        assert_eq!(result["failed"], 1);
        assert_eq!(result["results"][0]["value"], "first");
        assert_eq!(result["results"][0]["result_type"], 8);
        assert_eq!(result["results"][1]["error"], "unknown symbol");
        assert_eq!(result["results"][2]["value"], "last");
    }

    #[test]
    fn invalid_later_batch_item_prevents_every_read() {
        let mut calls = 0;
        let result = registers_with(vec!["PC".into(), "R0;Go".into()], |_| {
            calls += 1;
            Ok(0)
        });
        assert!(result.is_err());
        assert_eq!(calls, 0);

        let result = evaluate_with(
            EvaluateRequest {
                expressions: vec!["Register(PC)".into(), "\0".into()],
            },
            |_| {
                calls += 1;
                Ok(("0".into(), 8))
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 0);
    }

    #[test]
    fn registers_keep_full_width_and_partial_failures() {
        let result = registers_with(vec!["X0".into(), "bad".into()], |name| {
            if name == "X0" {
                Ok(u64::MAX)
            } else {
                Err("not found".into())
            }
        })
        .unwrap();
        assert_eq!(result["results"][0]["value"], u64::MAX);
        assert_eq!(result["results"][0]["hex"], "0xFFFFFFFFFFFFFFFF");
        assert_eq!(result["results"][1]["ok"], false);
    }

    #[test]
    fn address_parser_never_truncates_or_guesses_radix() {
        assert_eq!(parse_address("100").unwrap(), 100);
        assert_eq!(parse_address("0x100").unwrap(), 256);
        assert_eq!(parse_address("0XFFFFFFFF").unwrap(), u32::MAX);
        assert_eq!(parse_address("4294967295").unwrap(), u32::MAX);
        for invalid in [
            "0x100000000",
            "4294967296",
            "-1",
            "+1",
            "FF",
            "",
            "0x",
            "1.",
        ] {
            assert!(parse_address(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn memory_validates_all_regions_before_native_reads() {
        let mut calls = 0;
        let result = memory_with(
            MemoryRequest {
                regions: vec![region("0x1000", 1, None), region("0xFFFFFFFF", 2, None)],
            },
            |_, _, _, _| {
                calls += 1;
                Ok(vec![0])
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 0);

        let result = memory_with(
            MemoryRequest {
                regions: vec![region("0", 1, None), region("1", 1, Some("GG"))],
            },
            |_, _, _, _| {
                calls += 1;
                Ok(vec![0])
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 0);
    }

    #[test]
    fn memory_size_limits_and_last_address_are_checked() {
        assert!(MemoryRequest { regions: vec![] }.validate().is_err());
        for length in [0, MAX_REGION_BYTES + 1, usize::MAX] {
            assert!(
                MemoryRequest {
                    regions: vec![region("0", length, None)]
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            MemoryRequest {
                regions: (0..17).map(|_| region("0", 4096, None)).collect(),
            }
            .validate()
            .is_err()
        );
        assert!(
            MemoryRequest {
                regions: (0..16).map(|_| region("0", 4096, None)).collect(),
            }
            .validate()
            .is_ok()
        );
        assert!(
            MemoryRequest {
                regions: vec![region("0xFFFFFFFF", 1, None)]
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn expected_hex_supports_bytes_but_requires_exact_length() {
        assert_eq!(
            parse_expected_hex(" 0x01 ab\nFF ", 3).unwrap(),
            [1, 171, 255]
        );
        assert_eq!(parse_expected_hex("01abFF", 3).unwrap(), [1, 171, 255]);
        for invalid in ["0", "0000", "GG", "0x", "0x0G"] {
            assert!(parse_expected_hex(invalid, 1).is_err());
        }
    }

    #[test]
    fn memory_comparison_reports_full_mismatch_count_with_capped_offsets() {
        let mut input = region("0x1000", 70, Some(&"00".repeat(70)));
        input.access = MemoryAccess::ArmDap;
        input.dual_port = true;
        let result = memory_with(
            MemoryRequest {
                regions: vec![input],
            },
            |address, length, access, dual_port| {
                assert_eq!((address, length, access, dual_port), (0x1000, 70, 14, true));
                Ok(vec![1; length])
            },
        )
        .unwrap();
        let item = &result["results"][0];
        assert_eq!(item["matches"], false);
        assert_eq!(item["mismatch_count"], 70);
        assert_eq!(item["mismatch_offsets"].as_array().unwrap().len(), 64);
        assert_eq!(item["mismatch_offsets"][63], 63);
        assert_eq!(item["mismatch_offsets_truncated"], true);
        assert!(item["hexdump"].as_str().unwrap().starts_with("00001000"));
    }

    #[test]
    fn matching_memory_and_partial_read_errors_are_distinct() {
        let result = memory_with(
            MemoryRequest {
                regions: vec![region("0", 2, Some("41 00")), region("2", 2, Some("00 00"))],
            },
            |address, _, _, _| {
                if address == 0 {
                    Ok(vec![65, 0])
                } else {
                    Ok(vec![0])
                }
            },
        )
        .unwrap();
        assert_eq!(result["results"][0]["matches"], true);
        assert_eq!(result["results"][0]["hex"], "4100");
        assert!(
            result["results"][0]["hexdump"]
                .as_str()
                .unwrap()
                .contains("|A.|")
        );
        assert_eq!(result["results"][1]["ok"], false);
        assert!(result["results"][1].get("matches").is_none());
        assert_eq!(result["succeeded"], 1);
    }

    #[test]
    fn hexdump_preserves_columns_for_full_and_partial_rows() {
        let mut bytes: Vec<u8> = (0..16).collect();
        bytes.extend_from_slice(b"A \x7f");
        assert_eq!(
            hexdump(0x1000, &bytes),
            concat!(
                "00001000  00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F  |................|\n",
                "00001010  41 20 7F                                          |A .|\n",
            ),
        );
        assert_eq!(hexdump(0, &[]), "");
    }

    #[test]
    fn window_pagination_uses_native_bytes_not_utf8_length() {
        let result = window_with(
            WindowRequest {
                window: Window::Register,
                offset: 20,
                max_bytes: 1,
            },
            |command, offset, length| {
                assert_eq!((command, offset, length), ("Register.view", 20, 1));
                Ok(("\u{FFFD}".into(), 1))
            },
        )
        .unwrap();
        assert_eq!(result["bytes_read"], 1);
        assert_eq!(result["next_offset"], 21);
        assert_eq!(result["may_have_more"], true);
    }

    #[test]
    fn window_pagination_continues_full_and_short_pages_until_empty() {
        for text in ["full", "end", ""] {
            let bytes_read = text.len();
            let result = window_with(
                WindowRequest {
                    window: Window::Frame,
                    offset: 20,
                    max_bytes: 4,
                },
                |_, _, _| Ok((text.into(), bytes_read)),
            )
            .unwrap();
            assert_eq!(result["bytes_read"], bytes_read);
            assert_eq!(result["may_have_more"], bytes_read > 0);
            assert_eq!(
                result["next_offset"],
                if bytes_read > 0 {
                    json!(20 + bytes_read)
                } else {
                    Value::Null
                }
            );
        }
    }

    #[test]
    fn window_pagination_never_wraps_or_accepts_oversized_results() {
        let result = window_with(
            WindowRequest {
                window: Window::Frame,
                offset: u32::MAX - 1,
                max_bytes: 1,
            },
            |_, _, _| Ok(("x".into(), 1)),
        )
        .unwrap();
        assert_eq!(result["next_offset"], u32::MAX);
        assert_eq!(result["may_have_more"], true);

        let error = window_with(
            WindowRequest {
                window: Window::Frame,
                offset: u32::MAX - 1,
                max_bytes: 1,
            },
            |_, _, _| Ok(("xx".into(), 2)),
        )
        .unwrap_err();
        assert!(error.contains("more bytes than requested"));

        let error = window_with(
            WindowRequest {
                window: Window::Frame,
                offset: u32::MAX,
                max_bytes: 1,
            },
            |_, _, _| panic!("invalid bounds must be rejected before reading"),
        )
        .unwrap_err();
        assert!(error.contains("32-bit range"));
    }

    #[test]
    fn window_validation_rejects_unbounded_ranges_and_unknown_windows() {
        for (offset, max_bytes) in [(0, 0), (0, 8193), (u32::MAX, 1)] {
            assert!(
                WindowRequest {
                    window: Window::BreakList,
                    offset,
                    max_bytes
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<WindowRequest>(json!({"window": "Register.view;Go"})).is_err()
        );
        let request: WindowRequest =
            serde_json::from_value(json!({"window": "task_list"})).unwrap();
        assert_eq!(request.max_bytes, 4096);
        assert_eq!(request.offset, 0);
    }

    #[test]
    fn wait_defaults_and_limits_are_explicit() {
        let request: WaitRequest = serde_json::from_value(json!({
            "expression": "STATE.RUN()", "expected_value": "FALSE()",
        }))
        .unwrap();
        assert_eq!(request.timeout_ms, 5000);
        assert_eq!(request.interval_ms, 100);
        for timeout in [0, 60001, u64::MAX] {
            assert!(wait_request("1", timeout).validate().is_err());
        }
        let mut request = wait_request("1", 1000);
        for interval in [0, 19, 60001, u64::MAX] {
            request.interval_ms = interval;
            assert!(request.validate().is_err());
        }
    }

    #[tokio::test]
    async fn wait_matches_exact_text_and_reports_attempt_evidence() {
        let result = wait_with(wait_request("1.", 100), |_| Ok(("1.".into(), 8)))
            .await
            .unwrap();
        assert_eq!(result["matched"], true);
        assert_eq!(result["timed_out"], false);
        assert_eq!(result["attempts"], 1);
        assert_eq!(result["samples"][0]["value"], "1.");
        assert_eq!(result["samples"][0]["result_type"], 8);
        assert!(result["elapsed_ms"].is_number());

        let result = wait_with(wait_request("1.", 1), |_| Ok(("1".into(), 8)))
            .await
            .unwrap();
        assert_eq!(result["matched"], false);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["attempts"], 1);
        assert_eq!(result["timeout_scope"], "between_native_calls");
    }

    #[tokio::test]
    async fn wait_preserves_transient_errors_and_delays_retry() {
        let mut calls = 0;
        let result = wait_with(wait_request("ready", 1000), |_| {
            calls += 1;
            if calls == 1 {
                Err("temporarily unavailable".into())
            } else {
                Ok(("ready".into(), 2))
            }
        })
        .await
        .unwrap();
        assert_eq!(result["matched"], true);
        assert_eq!(result["attempts"], 2);
        assert_eq!(result["samples"][0]["error"], "temporarily unavailable");
        assert_eq!(result["samples"][1]["value"], "ready");
        assert!(result["elapsed_ms"].as_u64().unwrap() >= 20);
    }

    #[tokio::test]
    async fn invalid_wait_does_not_call_the_backend() {
        let mut calls = 0;
        let mut request = wait_request("ready", 100);
        request.expression = "1;Go".into();
        assert!(
            wait_with(request, |_| {
                calls += 1;
                Ok(("ready".into(), 2))
            })
            .await
            .is_err()
        );
        assert_eq!(calls, 0);
    }

    #[test]
    fn wait_retains_recent_samples_with_bounded_memory() {
        let mut samples = VecDeque::new();
        for attempt in 1..=1005 {
            retain_sample(&mut samples, json!({"attempt": attempt}));
        }
        assert_eq!(samples.len(), MAX_WAIT_SAMPLES);
        assert_eq!(samples.front().unwrap()["attempt"], 6);
        assert_eq!(samples.back().unwrap()["attempt"], 1005);
    }
}
