// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Bounded, sequential evidence capture and offline comparison. These helpers
//! never connect, halt, resume, or write the target. The caller serializes API use.

use crate::inspection::{
    EvaluateRequest, MemoryAccess, MemoryRegion, MemoryRequest, RegistersRequest, SymbolsRequest,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_ITEMS: usize = 64;
const MAX_NAME_BYTES: usize = 128;
const MAX_EXPECTED_TEXT_BYTES: usize = 4096;
const MAX_MEMORY_BYTES: usize = 65536;
const MAX_REQUEST_BYTES: usize = 512 * 1024;
const MAX_READ_BYTES: usize = 16 * 1024;
const MAX_ERROR_BYTES: usize = 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIFF_BYTES: usize = 2 * MAX_SNAPSHOT_BYTES;
const SNAPSHOT_FORMAT: &str = "t32mcp.snapshot";
const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureSnapshotRequest {
    /// Unique named checks, from 1 through 64; memory reads total at most 65536 bytes.
    pub items: Vec<SnapshotItem>,
    /// Ask the caller to persist the result locally. Capture itself performs no file I/O.
    #[serde(default)]
    pub save_artifact: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotItem {
    /// A stable, unique label used to match this item in another snapshot.
    pub name: String,
    pub query: SnapshotQuery,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotQuery {
    /// Observe only the numeric target state, without changing run state.
    Status { expected_state: Option<i32> },
    Expression {
        expression: String,
        /// Exact native result text, not a numeric coercion.
        expected_value: Option<String>,
    },
    Register {
        register: String,
        expected_value: Option<u64>,
    },
    Memory {
        /// Decimal or 0x-prefixed, within the unsigned 32-bit address space.
        address: String,
        /// From 1 through 4096 bytes.
        length: usize,
        #[serde(default)]
        access: MemoryAccess,
        /// Running-target reads require arm_dap plus dual_port=true (EDAP).
        #[serde(default)]
        dual_port: bool,
        /// Exactly length bytes, as contiguous or whitespace-separated hexadecimal.
        expected_hex: Option<String>,
    },
    Symbol {
        symbol: String,
        /// Optional expected 32-bit address, decimal or 0x-prefixed.
        expected_address: Option<String>,
    },
}

impl CaptureSnapshotRequest {
    /// Validate the entire capture before any state probe or native read occurs.
    pub fn validate(&self) -> Result<(), String> {
        validate_items(&self.items)?;
        if serialized_len(&self.items)? > MAX_REQUEST_BYTES {
            return Err(format!(
                "snapshot request exceeds the {MAX_REQUEST_BYTES}-byte metadata limit"
            ));
        }
        Ok(())
    }
}

fn validate_items(items: &[SnapshotItem]) -> Result<(), String> {
    if items.is_empty() || items.len() > MAX_ITEMS {
        return Err(format!(
            "items must contain between 1 and {MAX_ITEMS} items"
        ));
    }
    let mut names = HashSet::new();
    let mut memory_bytes = 0usize;
    for (index, item) in items.iter().enumerate() {
        if item.name.trim().is_empty()
            || item.name.len() > MAX_NAME_BYTES
            || item.name.chars().any(char::is_control)
        {
            return Err(format!(
                "items[{index}].name must be nonempty, at most {MAX_NAME_BYTES} bytes, and contain no control characters"
            ));
        }
        if !names.insert(&item.name) {
            return Err(format!("duplicate snapshot item name {:?}", item.name));
        }
        validate_query(&item.query).map_err(|error| format!("items[{index}]: {error}"))?;
        if let SnapshotQuery::Memory { length, .. } = &item.query {
            memory_bytes += length;
            if memory_bytes > MAX_MEMORY_BYTES {
                return Err(format!(
                    "snapshot memory length must not exceed {MAX_MEMORY_BYTES} bytes"
                ));
            }
        }
    }
    Ok(())
}

fn validate_query(query: &SnapshotQuery) -> Result<(), String> {
    match query {
        SnapshotQuery::Status { .. } => Ok(()),
        SnapshotQuery::Expression {
            expression,
            expected_value,
        } => {
            EvaluateRequest {
                expressions: vec![expression.clone()],
            }
            .validate()?;
            if expected_value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_EXPECTED_TEXT_BYTES)
            {
                return Err(format!(
                    "expected_value must not exceed {MAX_EXPECTED_TEXT_BYTES} bytes"
                ));
            }
            Ok(())
        }
        SnapshotQuery::Register { register, .. } => RegistersRequest {
            names: vec![register.clone()],
        }
        .validate(),
        SnapshotQuery::Memory {
            address,
            length,
            access,
            dual_port,
            expected_hex,
        } => MemoryRequest {
            regions: vec![MemoryRegion {
                address: address.clone(),
                length: *length,
                access: *access,
                dual_port: *dual_port,
                expected_hex: expected_hex.clone(),
            }],
        }
        .validate(),
        SnapshotQuery::Symbol {
            symbol,
            expected_address,
        } => {
            SymbolsRequest {
                names: vec![symbol.clone()],
            }
            .validate()?;
            if let Some(address) = expected_address {
                MemoryRequest {
                    regions: vec![MemoryRegion {
                        address: address.clone(),
                        length: 1,
                        access: MemoryAccess::Data,
                        dual_port: false,
                        expected_hex: None,
                    }],
                }
                .validate()?;
            }
            Ok(())
        }
    }
}

/// A test seam for captures. Successful reads return the query-specific value
/// object documented by NativeSnapshotBackend; capture validates its shape/size.
pub trait SnapshotBackend {
    fn state(&mut self) -> Result<i32, String>;
    fn read(&mut self, query: &SnapshotQuery) -> Result<Value, String>;
}

pub struct NativeSnapshotBackend;

impl SnapshotBackend for NativeSnapshotBackend {
    fn state(&mut self) -> Result<i32, String> {
        crate::t32_state()
    }

    fn read(&mut self, query: &SnapshotQuery) -> Result<Value, String> {
        match query {
            SnapshotQuery::Status { .. } => {
                crate::t32_state().map(|state| json!({ "state": state }))
            }
            SnapshotQuery::Expression { expression, .. } => crate::t32_eval(expression)
                .map(|result| json!({ "value": result.value, "result_type": result.result_type })),
            SnapshotQuery::Register { register, .. } => {
                crate::t32_read_register(register).map(|value| json!({ "value": value }))
            }
            SnapshotQuery::Memory {
                address,
                length,
                access,
                dual_port,
                ..
            } => {
                let bytes = crate::t32_read_memory(
                    parse_address(address)?,
                    *length,
                    access.native(),
                    *dual_port,
                )?;
                if bytes.len() != *length {
                    return Err(format!(
                        "memory read returned {} bytes; expected {length}",
                        bytes.len()
                    ));
                }
                let mut hex = String::with_capacity(bytes.len() * 2);
                for byte in bytes {
                    write!(&mut hex, "{byte:02X}").expect("writing to String cannot fail");
                }
                Ok(json!({ "hex": hex }))
            }
            SnapshotQuery::Symbol { symbol, .. } => {
                crate::t32_lookup_symbol(symbol).map(|symbol| {
                    json!({
                        "address": symbol.address,
                        "size": symbol.size,
                        "access": symbol.access,
                    })
                })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadObservation {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ReadObservation {
    fn success(value: Value) -> Self {
        Self {
            ok: true,
            value: Some(value),
            error: None,
        }
    }

    fn failure(error: String) -> Self {
        Self {
            ok: false,
            value: None,
            error: Some(bounded_error(error)),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.ok {
            if self.value.is_none() || self.error.is_some() {
                return Err("successful observation must have only a value".into());
            }
            if serialized_len(self.value.as_ref().expect("checked above"))? > MAX_READ_BYTES {
                return Err("observation value exceeds the read output limit".into());
            }
        } else if self.value.is_some()
            || self
                .error
                .as_ref()
                .is_none_or(|error| error.is_empty() || error.len() > MAX_ERROR_BYTES)
        {
            return Err("failed observation must have only a bounded, nonempty error".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CheckOutcome {
    Pass,
    Fail,
    NotRequested,
    Incomplete,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CheckCounts {
    pass: usize,
    fail: usize,
    not_requested: usize,
    incomplete: usize,
}

impl CheckCounts {
    fn add(&mut self, check: CheckOutcome) {
        match check {
            CheckOutcome::Pass => self.pass += 1,
            CheckOutcome::Fail => self.fail += 1,
            CheckOutcome::NotRequested => self.not_requested += 1,
            CheckOutcome::Incomplete => self.incomplete += 1,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CapturedItem {
    name: String,
    query: SnapshotQuery,
    started_unix_ms: u64,
    completed_unix_ms: u64,
    elapsed_ms: u64,
    read: ReadObservation,
    check: CheckOutcome,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotDocument {
    format: String,
    version: u32,
    started_unix_ms: u64,
    completed_unix_ms: u64,
    elapsed_ms: u64,
    non_atomic: bool,
    state_before: ReadObservation,
    state_after: ReadObservation,
    count: usize,
    succeeded: usize,
    failed: usize,
    checks: CheckCounts,
    items: Vec<CapturedItem>,
}

pub fn capture(request: CaptureSnapshotRequest) -> Result<Value, String> {
    capture_with(request, &mut NativeSnapshotBackend)
}

/// Preserve independent results and failed expectations instead of making one
/// unsuccessful check discard otherwise useful evidence.
pub fn capture_with(
    request: CaptureSnapshotRequest,
    backend: &mut impl SnapshotBackend,
) -> Result<Value, String> {
    request.validate()?;
    let started_unix_ms = unix_ms();
    let started = Instant::now();
    let state_before = observe_state(backend.state());
    let mut items = Vec::with_capacity(request.items.len());
    let mut checks = CheckCounts::default();
    let mut succeeded = 0;
    for item in request.items {
        let item_started_unix_ms = unix_ms();
        let item_started = Instant::now();
        let read = match backend
            .read(&item.query)
            .and_then(|value| validate_value(&item.query, value))
        {
            Ok(value) => ReadObservation::success(value),
            Err(error) => ReadObservation::failure(error),
        };
        let check = evaluate_check(&item.query, &read)?;
        checks.add(check);
        succeeded += usize::from(read.ok);
        items.push(CapturedItem {
            name: item.name,
            query: item.query,
            started_unix_ms: item_started_unix_ms,
            completed_unix_ms: unix_ms(),
            elapsed_ms: elapsed_ms(item_started),
            read,
            check,
        });
    }
    let state_after = observe_state(backend.state());
    let document = SnapshotDocument {
        format: SNAPSHOT_FORMAT.into(),
        version: SNAPSHOT_VERSION,
        started_unix_ms,
        completed_unix_ms: unix_ms(),
        elapsed_ms: elapsed_ms(started),
        // Even a halted target is not a promise that every memory source is static.
        non_atomic: true,
        state_before,
        state_after,
        count: items.len(),
        succeeded,
        failed: items.len() - succeeded,
        checks,
        items,
    };
    let value = serde_json::to_value(document).map_err(|error| error.to_string())?;
    if serialized_len(&value)? > MAX_SNAPSHOT_BYTES {
        return Err("snapshot exceeds the serialized output limit".into());
    }
    Ok(value)
}

/// Turn a connection failure into bounded per-item evidence without calling the API.
pub fn capture_connection_failure(
    request: CaptureSnapshotRequest,
    error: String,
) -> Result<Value, String> {
    struct UnavailableBackend(String);
    impl SnapshotBackend for UnavailableBackend {
        fn state(&mut self) -> Result<i32, String> {
            Err(self.0.clone())
        }

        fn read(&mut self, _query: &SnapshotQuery) -> Result<Value, String> {
            Err(self.0.clone())
        }
    }
    capture_with(request, &mut UnavailableBackend(bounded_error(error)))
}

fn observe_state(result: Result<i32, String>) -> ReadObservation {
    match result {
        Ok(state) => ReadObservation::success(json!({ "state": state })),
        Err(error) => ReadObservation::failure(error),
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StatusValue {
    state: i32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExpressionValue {
    value: String,
    result_type: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegisterValue {
    value: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MemoryValue {
    hex: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SymbolValue {
    address: u32,
    size: u32,
    access: u32,
}

fn validate_value(query: &SnapshotQuery, value: Value) -> Result<Value, String> {
    if serialized_len(&value)? > MAX_READ_BYTES {
        return Err(format!(
            "read output exceeds the {MAX_READ_BYTES}-byte limit; value was not retained"
        ));
    }
    let invalid = |error| format!("invalid snapshot read result: {error}");
    match query {
        SnapshotQuery::Status { .. } => {
            let parsed: StatusValue = serde_json::from_value(value).map_err(invalid)?;
            Ok(json!({ "state": parsed.state }))
        }
        SnapshotQuery::Expression { .. } => {
            let parsed: ExpressionValue = serde_json::from_value(value).map_err(invalid)?;
            Ok(json!({ "value": parsed.value, "result_type": parsed.result_type }))
        }
        SnapshotQuery::Register { .. } => {
            let parsed: RegisterValue = serde_json::from_value(value).map_err(invalid)?;
            Ok(json!({ "value": parsed.value }))
        }
        SnapshotQuery::Memory { length, .. } => {
            let mut parsed: MemoryValue = serde_json::from_value(value).map_err(invalid)?;
            if parsed.hex.len() != length * 2 || !parsed.hex.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(format!(
                    "memory read result must contain exactly {length} bytes of hexadecimal"
                ));
            }
            parsed.hex.make_ascii_uppercase();
            Ok(json!({ "hex": parsed.hex }))
        }
        SnapshotQuery::Symbol { .. } => {
            let parsed: SymbolValue = serde_json::from_value(value).map_err(invalid)?;
            if parsed.address == u32::MAX && parsed.size == u32::MAX {
                return Err("symbol result contains the documented missing-symbol sentinel".into());
            }
            Ok(json!({
                "address": parsed.address,
                "size": parsed.size,
                "access": parsed.access,
            }))
        }
    }
}

fn expected_value(query: &SnapshotQuery) -> Result<Option<Value>, String> {
    Ok(match query {
        SnapshotQuery::Status { expected_state } => expected_state.map(|value| json!(value)),
        SnapshotQuery::Expression { expected_value, .. } => {
            expected_value.as_ref().map(|value| json!(value))
        }
        SnapshotQuery::Register { expected_value, .. } => expected_value.map(|value| json!(value)),
        SnapshotQuery::Memory { expected_hex, .. } => {
            expected_hex.as_ref().map(|hex| json!(canonical_hex(hex)))
        }
        SnapshotQuery::Symbol {
            expected_address, ..
        } => expected_address
            .as_deref()
            .map(parse_address)
            .transpose()?
            .map(|value| json!(value)),
    })
}

fn evaluate_check(query: &SnapshotQuery, read: &ReadObservation) -> Result<CheckOutcome, String> {
    let Some(expected) = expected_value(query)? else {
        return Ok(CheckOutcome::NotRequested);
    };
    if !read.ok {
        return Ok(CheckOutcome::Incomplete);
    }
    let value = read
        .value
        .as_ref()
        .ok_or_else(|| "successful read has no value".to_string())?;
    let actual = match query {
        SnapshotQuery::Status { .. } => &value["state"],
        SnapshotQuery::Expression { .. } | SnapshotQuery::Register { .. } => &value["value"],
        SnapshotQuery::Memory { .. } => &value["hex"],
        SnapshotQuery::Symbol { .. } => &value["address"],
    };
    Ok(if *actual == expected {
        CheckOutcome::Pass
    } else {
        CheckOutcome::Fail
    })
}

fn canonical_hex(text: &str) -> String {
    let trimmed = text.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    digits
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .map(|byte| (byte as char).to_ascii_uppercase())
        .collect()
}

fn parse_address(text: &str) -> Result<u32, String> {
    let text = text.trim();
    let (digits, radix) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(digits) => (digits, 16),
        None => (text, 10),
    };
    u32::from_str_radix(digits, radix).map_err(|_| "invalid 32-bit address".into())
}

fn query_identity(query: &SnapshotQuery) -> Result<Value, String> {
    Ok(match query {
        SnapshotQuery::Status { .. } => json!({ "kind": "status" }),
        SnapshotQuery::Expression { expression, .. } => {
            json!({ "kind": "expression", "expression": expression })
        }
        SnapshotQuery::Register { register, .. } => {
            json!({ "kind": "register", "register": register })
        }
        SnapshotQuery::Memory {
            address,
            length,
            access,
            dual_port,
            ..
        } => json!({
            "kind": "memory",
            "address": parse_address(address)?,
            "length": length,
            "access": access,
            "dual_port": dual_port,
        }),
        SnapshotQuery::Symbol { symbol, .. } => json!({ "kind": "symbol", "symbol": symbol }),
    })
}

fn validate_document(value: &Value) -> Result<SnapshotDocument, String> {
    if serialized_len(value)? > MAX_SNAPSHOT_BYTES {
        return Err("snapshot payload exceeds the serialized input limit".into());
    }
    let mut document: SnapshotDocument = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid snapshot: {error}"))?;
    if document.format != SNAPSHOT_FORMAT
        || document.version != SNAPSHOT_VERSION
        || !document.non_atomic
    {
        return Err("unsupported snapshot format/version or invalid atomicity claim".into());
    }
    let request = CaptureSnapshotRequest {
        items: document
            .items
            .iter()
            .map(|item| SnapshotItem {
                name: item.name.clone(),
                query: item.query.clone(),
            })
            .collect(),
        save_artifact: false,
    };
    request.validate()?;
    for state in [&document.state_before, &document.state_after] {
        state.validate()?;
        if let Some(value) = &state.value {
            validate_value(
                &SnapshotQuery::Status {
                    expected_state: None,
                },
                value.clone(),
            )?;
        }
    }
    let mut succeeded = 0;
    let mut checks = CheckCounts::default();
    for item in &mut document.items {
        item.read.validate()?;
        if let Some(value) = &mut item.read.value {
            *value = validate_value(&item.query, value.clone())?;
        }
        if item.check != evaluate_check(&item.query, &item.read)? {
            return Err(format!(
                "snapshot item {:?} has an inconsistent check outcome",
                item.name
            ));
        }
        succeeded += usize::from(item.read.ok);
        checks.add(item.check);
    }
    if document.count != document.items.len()
        || document.succeeded != succeeded
        || document.failed != document.items.len() - succeeded
        || document.checks != checks
    {
        return Err("snapshot summary does not match its item results".into());
    }
    Ok(document)
}

fn diff_side(item: &CapturedItem) -> Value {
    json!({ "query": item.query, "read": item.read, "check": item.check })
}

/// Compare validated saved payloads without reading the target. Failure on either
/// side is unavailable evidence, never proof that values are unchanged.
pub fn diff_snapshots(before: &Value, after: &Value) -> Result<Value, String> {
    let before = validate_document(before).map_err(|error| format!("before: {error}"))?;
    let after = validate_document(after).map_err(|error| format!("after: {error}"))?;
    let after_by_name: HashMap<&str, &CapturedItem> = after
        .items
        .iter()
        .map(|item| (item.name.as_str(), item))
        .collect();
    let before_names: HashSet<&str> = before.items.iter().map(|item| item.name.as_str()).collect();
    let mut results = Vec::with_capacity(before.items.len() + after.items.len());
    let mut counts = json!({
        "added": 0,
        "removed": 0,
        "changed": 0,
        "unchanged": 0,
        "incompatible": 0,
        "unavailable": 0,
    });
    for old in &before.items {
        let Some(new) = after_by_name.get(old.name.as_str()) else {
            counts["removed"] = json!(counts["removed"].as_u64().unwrap_or(0) + 1);
            results.push(json!({
                "name": old.name,
                "change": "removed",
                "before": diff_side(old),
            }));
            continue;
        };
        let compatible = query_identity(&old.query)? == query_identity(&new.query)?;
        let change = if !compatible {
            "incompatible"
        } else if !old.read.ok || !new.read.ok {
            "unavailable"
        } else if old.read.value == new.read.value {
            "unchanged"
        } else {
            "changed"
        };
        counts[change] = json!(counts[change].as_u64().unwrap_or(0) + 1);
        let mut item = json!({
            "name": old.name,
            "change": change,
            "expectation_changed": expected_value(&old.query)? != expected_value(&new.query)?,
            "check_changed": old.check != new.check,
        });
        // Identical observations need only a label, rather than repeating large
        // memory blocks twice. Changed expectations still retain both checks.
        if change != "unchanged" || item["expectation_changed"] == true {
            item["before"] = diff_side(old);
            item["after"] = diff_side(new);
        }
        results.push(item);
    }
    for new in &after.items {
        if !before_names.contains(new.name.as_str()) {
            counts["added"] = json!(counts["added"].as_u64().unwrap_or(0) + 1);
            results.push(json!({
                "name": new.name,
                "change": "added",
                "after": diff_side(new),
            }));
        }
    }
    let result = json!({
        "format": "t32mcp.snapshot_diff",
        "version": 1,
        "non_atomic": true,
        "before_started_unix_ms": before.started_unix_ms,
        "after_started_unix_ms": after.started_unix_ms,
        "count": results.len(),
        "counts": counts,
        "items": results,
    });
    if serialized_len(&result)? > MAX_DIFF_BYTES {
        return Err("snapshot diff exceeds the serialized output limit".into());
    }
    Ok(result)
}

fn serialized_len(value: &impl Serialize) -> Result<usize, String> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|error| format!("cannot serialize snapshot data: {error}"))
}

fn bounded_error(mut error: String) -> String {
    if error.is_empty() {
        return "unspecified read failure".into();
    }
    if error.len() > MAX_ERROR_BYTES {
        let suffix = "... [truncated]";
        let mut end = MAX_ERROR_BYTES - suffix.len();
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
        error.push_str(suffix);
    }
    error
}

fn unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeBackend {
        states: VecDeque<Result<i32, String>>,
        reads: VecDeque<Result<Value, String>>,
        calls: usize,
    }

    impl FakeBackend {
        fn new(reads: Vec<Result<Value, String>>) -> Self {
            Self {
                states: VecDeque::from([Ok(3), Ok(3)]),
                reads: reads.into(),
                calls: 0,
            }
        }
    }

    impl SnapshotBackend for FakeBackend {
        fn state(&mut self) -> Result<i32, String> {
            self.calls += 1;
            self.states.pop_front().expect("expected state call")
        }

        fn read(&mut self, _query: &SnapshotQuery) -> Result<Value, String> {
            self.calls += 1;
            self.reads.pop_front().expect("expected read call")
        }
    }

    fn request(items: Value) -> CaptureSnapshotRequest {
        serde_json::from_value(json!({ "items": items })).unwrap()
    }

    fn register_item(name: &str, register: &str, expected_value: Option<u64>) -> Value {
        json!({
            "name": name,
            "query": { "kind": "register", "register": register, "expected_value": expected_value },
        })
    }

    fn capture_register(name: &str, value: Result<u64, &str>, expected: Option<u64>) -> Value {
        capture_with(
            request(json!([register_item(name, "PC", expected)])),
            &mut FakeBackend::new(vec![
                value
                    .map(|value| json!({ "value": value }))
                    .map_err(str::to_string),
            ]),
        )
        .unwrap()
    }

    #[test]
    fn validates_every_item_before_any_backend_call() {
        let mut backend = FakeBackend::new(vec![]);
        let result = capture_with(
            request(json!([
                register_item("valid", "PC", None),
                register_item("invalid", "PC;Go", None),
            ])),
            &mut backend,
        );
        assert!(result.is_err());
        assert_eq!(backend.calls, 0);
    }

    #[test]
    fn names_counts_and_unknown_fields_are_checked() {
        for items in [
            json!([]),
            json!([register_item("", "PC", None)]),
            json!([register_item("name\n", "PC", None)]),
            json!([register_item(&"x".repeat(MAX_NAME_BYTES + 1), "PC", None)]),
            json!([
                register_item("same", "PC", None),
                register_item("same", "SP", None)
            ]),
            json!(
                (0..65)
                    .map(|n| register_item(&format!("r{n}"), "PC", None))
                    .collect::<Vec<_>>()
            ),
        ] {
            assert!(request(items).validate().is_err());
        }
        assert!(
            serde_json::from_value::<CaptureSnapshotRequest>(json!({
                "items": [{ "name": "a", "query": {"kind": "status", "reset": true} }]
            }))
            .is_err()
        );
    }

    #[test]
    fn aggregate_memory_and_later_hex_are_validated_before_state_probe() {
        let items: Vec<Value> = (0..17)
            .map(|index| {
                json!({
                    "name": format!("memory{index}"),
                    "query": {"kind": "memory", "address": "0", "length": 4096}
                })
            })
            .collect();
        let mut backend = FakeBackend::new(vec![]);
        assert!(capture_with(request(json!(items)), &mut backend).is_err());
        assert_eq!(backend.calls, 0);

        assert!(
            capture_with(
                request(json!([
                    register_item("pc", "PC", None),
                    {"name": "bad", "query": {"kind":"memory","address":"0","length":1,"expected_hex":"GG"}}
                ])),
                &mut backend
            )
            .is_err()
        );
        assert_eq!(backend.calls, 0);
    }

    #[test]
    fn snapshot_preserves_independent_reads_and_check_outcomes() {
        let mut backend = FakeBackend::new(vec![
            Ok(json!({"value": 7})),
            Ok(json!({"value": 2})),
            Err("register unavailable while running".into()),
            Ok(json!({"value": u64::MAX})),
        ]);
        backend.states = VecDeque::from([Ok(2), Ok(3)]);
        let result = capture_with(
            request(json!([
                register_item("match", "R0", Some(7)),
                register_item("mismatch", "R1", Some(7)),
                register_item("unavailable", "PC", Some(7)),
                register_item("without_check", "R2", None),
            ])),
            &mut backend,
        )
        .unwrap();
        assert_eq!(result["non_atomic"], true);
        assert_eq!(result["state_before"]["value"]["state"], 2);
        assert_eq!(result["state_after"]["value"]["state"], 3);
        assert_eq!(result["count"], 4);
        assert_eq!(result["succeeded"], 3);
        assert_eq!(result["failed"], 1);
        assert_eq!(
            result["checks"],
            json!({"pass": 1, "fail": 1, "not_requested": 1, "incomplete": 1})
        );
        assert_eq!(result["items"][0]["check"], "pass");
        assert_eq!(result["items"][1]["check"], "fail");
        assert_eq!(result["items"][2]["check"], "incomplete");
        assert_eq!(result["items"][3]["read"]["value"]["value"], u64::MAX);
        assert_eq!(backend.calls, 6);
        assert!(validate_document(&result).is_ok());
    }

    #[test]
    fn expression_checks_are_exact_text_and_retain_native_type() {
        let result = capture_with(
            request(json!([{
                "name": "expr",
                "query": {"kind": "expression", "expression": "1", "expected_value": "1"}
            }])),
            &mut FakeBackend::new(vec![Ok(json!({"value": "1.", "result_type": 8}))]),
        )
        .unwrap();
        assert_eq!(result["items"][0]["read"]["value"]["result_type"], 8);
        assert_eq!(result["items"][0]["check"], "fail");
    }

    #[test]
    fn memory_and_symbol_expectations_use_normalized_addresses_and_hex() {
        let result = capture_with(
            request(json!([
                {"name": "bytes", "query": {"kind":"memory","address":"0x10","length":2,"expected_hex":"0x aa FF"}},
                {"name": "symbol", "query": {"kind":"symbol","symbol":"main","expected_address":"16"}},
                {"name": "state", "query": {"kind":"status","expected_state":3}}
            ])),
            &mut FakeBackend::new(vec![
                Ok(json!({"hex":"aaff"})),
                Ok(json!({"address":16,"size":4,"access":0})),
                Ok(json!({"state":3})),
            ]),
        )
        .unwrap();
        assert_eq!(result["checks"]["pass"], 3);
        assert_eq!(result["items"][0]["read"]["value"]["hex"], "AAFF");
        assert!(validate_document(&result).is_ok());
    }

    #[test]
    fn connection_failure_keeps_all_requested_checks_as_incomplete() {
        let result = capture_connection_failure(
            request(json!([
                register_item("pc", "PC", Some(0)),
                {"name":"bytes","query":{"kind":"memory","address":"0","length":1,"expected_hex":"00"}},
                register_item("sp", "SP", None),
            ])),
            "cannot connect".into(),
        )
        .unwrap();
        assert_eq!(result["succeeded"], 0);
        assert_eq!(result["failed"], 3);
        assert_eq!(result["checks"]["incomplete"], 2);
        assert_eq!(result["checks"]["not_requested"], 1);
        assert_eq!(result["state_before"]["ok"], false);
        assert_eq!(result["state_after"]["ok"], false);
        assert!(validate_document(&result).is_ok());
    }

    #[test]
    fn invalid_or_overlarge_read_values_are_explicit_failed_observations() {
        for value in [
            json!({"value": -1}),
            json!({"value": 7, "unexpected": true}),
            json!({"value": "not a register"}),
            json!({"value": "x".repeat(MAX_READ_BYTES)}),
        ] {
            let result = capture_with(
                request(json!([register_item("pc", "PC", Some(7))])),
                &mut FakeBackend::new(vec![Ok(value)]),
            )
            .unwrap();
            assert_eq!(result["failed"], 1);
            assert_eq!(result["items"][0]["check"], "incomplete");
            assert!(result["items"][0]["read"].get("value").is_none());
            assert!(validate_document(&result).is_ok());
        }
    }

    #[test]
    fn short_memory_and_missing_symbol_sentinel_are_not_successes() {
        let result = capture_with(
            request(json!([
                {"name":"bytes","query":{"kind":"memory","address":"0","length":2,"expected_hex":"0000"}},
                {"name":"symbol","query":{"kind":"symbol","symbol":"missing","expected_address":"0"}}
            ])),
            &mut FakeBackend::new(vec![
                Ok(json!({"hex":"00"})),
                Ok(json!({"address":u32::MAX,"size":u32::MAX,"access":0})),
            ]),
        )
        .unwrap();
        assert_eq!(result["failed"], 2);
        assert_eq!(result["checks"]["incomplete"], 2);
    }

    #[test]
    fn errors_are_nonempty_and_bounded_at_utf8_boundaries() {
        for error in ["".to_string(), "\u{4E2D}".repeat(1000)] {
            let result = capture_connection_failure(
                request(json!([register_item("pc", "PC", None)])),
                error,
            )
            .unwrap();
            let error = result["items"][0]["read"]["error"].as_str().unwrap();
            assert!(!error.is_empty());
            assert!(error.len() <= MAX_ERROR_BYTES);
            assert!(validate_document(&result).is_ok());
        }
    }

    #[test]
    fn captures_have_per_item_timing_and_explicit_non_atomicity() {
        let result = capture_register("pc", Ok(9), None);
        for key in ["started_unix_ms", "completed_unix_ms", "elapsed_ms"] {
            assert!(result[key].is_u64());
            assert!(result["items"][0][key].is_u64());
        }
        assert_eq!(result["non_atomic"], true);
        assert!(!request(json!([register_item("pc", "PC", None)])).save_artifact);
    }

    #[test]
    fn request_serialization_bound_is_checked_before_reads() {
        let items: Vec<Value> = (0..64)
            .map(|n| json!({
                "name":format!("item{n}"),
                "query":{"kind":"expression","expression":"1","expected_value":"\0".repeat(4096)}
            }))
            .collect();
        let mut backend = FakeBackend::new(vec![]);
        let error = capture_with(request(json!(items)), &mut backend).unwrap_err();
        assert!(error.contains("metadata limit"));
        assert_eq!(backend.calls, 0);
    }

    #[test]
    fn maximal_bounded_reads_fit_snapshot_output_limit() {
        let items: Vec<Value> = (0..64)
            .map(|n| json!({
                "name":format!("item{n}"),
                "query":{"kind":"expression","expression":"x".repeat(4096),"expected_value":"y".repeat(3000)}
            }))
            .collect();
        let result = capture_with(
            request(json!(items)),
            &mut FakeBackend::new(vec![
                Ok(json!({"value":"z".repeat(16000),"result_type":8}));
                64
            ]),
        )
        .unwrap();
        assert_eq!(result["succeeded"], 64);
        assert!(serialized_len(&result).unwrap() <= MAX_SNAPSHOT_BYTES);
    }

    #[test]
    fn diff_reports_changed_unchanged_added_and_removed_by_unique_names() {
        let before = capture_with(
            request(json!([
                register_item("changed", "PC", None),
                register_item("same", "SP", None),
                register_item("removed", "R0", None),
            ])),
            &mut FakeBackend::new(vec![
                Ok(json!({"value":1})),
                Ok(json!({"value":2})),
                Ok(json!({"value":3})),
            ]),
        )
        .unwrap();
        let after = capture_with(
            request(json!([
                register_item("same", "SP", None),
                register_item("changed", "PC", None),
                register_item("added", "R1", None),
            ])),
            &mut FakeBackend::new(vec![
                Ok(json!({"value":2})),
                Ok(json!({"value":4})),
                Ok(json!({"value":5})),
            ]),
        )
        .unwrap();
        let diff = diff_snapshots(&before, &after).unwrap();
        assert_eq!(
            diff["counts"],
            json!({
                "changed":1, "unchanged":1, "added":1, "removed":1, "incompatible":0, "unavailable":0
            })
        );
        assert_eq!(diff["items"][0]["name"], "changed");
        assert_eq!(diff["items"][3]["name"], "added");
        assert!(diff["items"][1].get("before").is_none());
        assert!(diff["items"][1].get("after").is_none());
    }

    #[test]
    fn diff_never_calls_two_unreadable_items_unchanged() {
        let before = capture_register("pc", Err("not available"), Some(0));
        let after = capture_register("pc", Err("not available"), Some(0));
        let diff = diff_snapshots(&before, &after).unwrap();
        assert_eq!(diff["counts"]["unchanged"], 0);
        assert_eq!(diff["counts"]["unavailable"], 1);
        assert_eq!(diff["items"][0]["before"]["read"]["error"], "not available");
        let readable = capture_register("pc", Ok(0), Some(0));
        assert_eq!(
            diff_snapshots(&before, &readable).unwrap()["counts"]["unavailable"],
            1
        );
    }

    #[test]
    fn diff_does_not_compare_values_from_incompatible_query_semantics() {
        let before = capture_register("pc", Ok(7), None);
        let mut after = before.clone();
        after["items"][0]["query"]["register"] = json!("SP");
        let diff = diff_snapshots(&before, &after).unwrap();
        assert_eq!(diff["counts"]["incompatible"], 1);
        assert_eq!(diff["counts"]["unchanged"], 0);
    }

    #[test]
    fn diff_ignores_timing_but_reports_changed_expectations_separately() {
        let before = capture_register("pc", Ok(7), Some(7));
        let mut after = capture_register("pc", Ok(7), Some(8));
        after["started_unix_ms"] = json!(42);
        after["completed_unix_ms"] = json!(1);
        after["elapsed_ms"] = json!(999);
        after["items"][0]["elapsed_ms"] = json!(8);
        let diff = diff_snapshots(&before, &after).unwrap();
        assert_eq!(diff["counts"]["unchanged"], 1);
        assert_eq!(diff["items"][0]["expectation_changed"], true);
        assert_eq!(diff["items"][0]["check_changed"], true);
    }

    #[test]
    fn diff_normalizes_memory_address_spelling_but_preserves_access_semantics() {
        let before = capture_with(
            request(json!([{
                "name":"bytes","query":{"kind":"memory","address":"16","length":1}
            }])),
            &mut FakeBackend::new(vec![Ok(json!({"hex":"ab"}))]),
        )
        .unwrap();
        let mut after = before.clone();
        after["items"][0]["query"]["address"] = json!("0x00000010");
        after["items"][0]["read"]["value"]["hex"] = json!("ab");
        assert_eq!(
            diff_snapshots(&before, &after).unwrap()["counts"]["unchanged"],
            1
        );
        after["items"][0]["query"]["dual_port"] = json!(true);
        assert_eq!(
            diff_snapshots(&before, &after).unwrap()["counts"]["incompatible"],
            1
        );
    }

    #[test]
    fn diff_rejects_malformed_forged_and_future_payloads() {
        let original = capture_register("pc", Ok(7), Some(7));
        for (path, value) in [
            (vec!["version"], json!(2)),
            (vec!["format"], json!("other")),
            (vec!["non_atomic"], json!(false)),
            (vec!["count"], json!(2)),
            (vec!["succeeded"], json!(0)),
            (vec!["checks", "pass"], json!(0)),
        ] {
            let mut malformed = original.clone();
            let mut destination = &mut malformed;
            for field in path {
                destination = &mut destination[field];
            }
            *destination = value;
            assert!(diff_snapshots(&original, &malformed).is_err());
        }
        let mut malformed = original.clone();
        malformed["items"][0]["check"] = json!("fail");
        assert!(diff_snapshots(&original, &malformed).is_err());
        malformed = original.clone();
        malformed["items"][0]["read"]["error"] = json!("hidden error on successful read");
        assert!(diff_snapshots(&original, &malformed).is_err());
        malformed = original.clone();
        malformed["items"]
            .as_array_mut()
            .unwrap()
            .push(original["items"][0].clone());
        assert!(diff_snapshots(&original, &malformed).is_err());
        malformed = original.clone();
        malformed["unexpected"] = json!(true);
        assert!(diff_snapshots(&original, &malformed).is_err());
        assert!(diff_snapshots(&original, &json!({})).is_err());
    }

    #[test]
    fn diff_rejects_oversized_payload_before_deserializing_schema() {
        let original = capture_register("pc", Ok(7), None);
        let oversized = json!({"irrelevant":"x".repeat(MAX_SNAPSHOT_BYTES)});
        let error = diff_snapshots(&original, &oversized).unwrap_err();
        assert!(error.contains("input limit"));
    }
}
