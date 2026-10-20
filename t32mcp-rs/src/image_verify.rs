// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Local image readback verification, without loading, halting, or writing.
//! `prepare` performs all file and request validation without accessing TRACE32.
//! Callers must serialize native access and connect only after preparation.
//! Allowlisted addresses reduce accidents, not MMIO side effects or TOCTOU risk.

use crate::inspection::MemoryAccess;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SEGMENTS: usize = 1024;
const MAX_HEX_RECORDS: usize = 1_100_000;
const MAX_ALLOWED_RANGES: usize = 64;
const MAX_CHUNKS: usize = 16384;
const MAX_CHUNK_BYTES: usize = 4096;
const MAX_MISMATCHES: usize = 256;
const MAX_ERROR_BYTES: usize = 2048;
const ADDRESS_SPACE_END: u64 = 1u64 << 32;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Bin,
    IntelHex,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AllowedAddressRange {
    /// Unsigned 32-bit address: decimal digits or 0x-prefixed hexadecimal.
    pub address: String,
    /// Positive byte length. The exclusive end may equal 0x100000000.
    pub length: u64,
}

fn default_chunk_bytes() -> usize {
    MAX_CHUNK_BYTES
}

fn default_max_image_bytes() -> usize {
    1024 * 1024
}

fn default_timeout_ms() -> u64 {
    30000
}

fn default_max_mismatches() -> usize {
    64
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifyImageRequest {
    /// Absolute local regular file. Network, device and symlink/reparse paths
    /// are rejected. File input is bounded to 64 MiB.
    pub path: String,
    /// Explicit input format. Intel HEX holes are not read or filled.
    pub format: ImageFormat,
    /// Required for BIN; forbidden for Intel HEX (addresses are not relocated).
    pub base_address: Option<String>,
    /// Explicitly approved address ranges, 1..64. Exclude MMIO and sensitive
    /// regions. This is not a guarantee that reads are free of side effects.
    pub allowed_ranges: Vec<AllowedAddressRange>,
    #[serde(default)]
    pub access: MemoryAccess,
    /// Bytes per native read, 1..4096; total chunk count must not exceed 16384.
    #[serde(default = "default_chunk_bytes")]
    pub chunk_bytes: usize,
    /// Decoded-record byte limit, 1..16777216 (default 1 MiB). Identical HEX
    /// overlap is allowed but repeated record bytes still count toward the cap.
    #[serde(default = "default_max_image_bytes")]
    pub max_image_bytes: usize,
    /// Soft deadline, 1..60000 ms, checked between calls. A blocked native
    /// function cannot be interrupted by this deadline.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Retained mismatch addresses, 0..256. Counts still include all checked
    /// mismatches; target byte values are never included.
    #[serde(default = "default_max_mismatches")]
    pub max_mismatches: usize,
    /// Ask the server to persist the result metadata locally. No raw target
    /// bytes are included. Persistence is handled by the calling server.
    #[serde(default)]
    pub save_artifact: bool,
}

#[derive(Debug)]
struct Segment {
    address: u32,
    bytes: Vec<u8>,
}

impl Segment {
    fn end(&self) -> u64 {
        u64::from(self.address) + self.bytes.len() as u64
    }
}

/// Validated immutable image bytes. Construction is restricted to `prepare`;
/// verification never reopens the input path.
#[derive(Debug)]
pub struct PreparedImage {
    path: PathBuf,
    format: ImageFormat,
    file_bytes: usize,
    input_sha256: String,
    payload_sha256: String,
    segments: Vec<Segment>,
    allowed_ranges: Vec<(u64, u64)>,
    entry_point: Option<u32>,
    total_bytes: usize,
    chunk_count: usize,
    access: MemoryAccess,
    chunk_bytes: usize,
    timeout_ms: u64,
    max_mismatches: usize,
}

fn parse_address(text: &str) -> Result<u32, String> {
    if text.is_empty() || text.len() > 32 || text.trim() != text {
        return Err(
            "address must contain at most 32 characters and no surrounding whitespace".into(),
        );
    }
    let (digits, radix) = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .map_or((text, 10), |digits| (digits, 16));
    if digits.is_empty()
        || !digits.bytes().all(|byte| {
            if radix == 16 {
                byte.is_ascii_hexdigit()
            } else {
                byte.is_ascii_digit()
            }
        })
    {
        return Err("address must be decimal digits or 0x-prefixed hexadecimal".into());
    }
    u32::from_str_radix(digits, radix)
        .map_err(|_| "address exceeds the unsigned 32-bit range".into())
}

fn prepare_allowed_ranges(ranges: &[AllowedAddressRange]) -> Result<Vec<(u64, u64)>, String> {
    if ranges.is_empty() || ranges.len() > MAX_ALLOWED_RANGES {
        return Err(format!(
            "allowed_ranges must contain between 1 and {MAX_ALLOWED_RANGES} ranges"
        ));
    }
    let mut parsed = Vec::with_capacity(ranges.len());
    for (index, range) in ranges.iter().enumerate() {
        let start = u64::from(
            parse_address(&range.address)
                .map_err(|error| format!("allowed_ranges[{index}]: {error}"))?,
        );
        if range.length == 0 || range.length > ADDRESS_SPACE_END - start {
            return Err(format!(
                "allowed_ranges[{index}] must be nonempty and fit the 32-bit address space"
            ));
        }
        parsed.push((start, start + range.length));
    }
    parsed.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in parsed {
        if let Some(previous) = merged.last_mut()
            && start <= previous.1
        {
            previous.1 = previous.1.max(end);
            continue;
        }
        merged.push((start, end));
    }
    Ok(merged)
}

fn validate_path_text(text: &str) -> Result<(), String> {
    if text.is_empty() || text.len() > 4096 || text.chars().any(char::is_control) {
        return Err("image path must contain 1..4096 bytes and no control characters".into());
    }
    #[cfg(windows)]
    let disk_text = text.strip_prefix("\\\\?\\").unwrap_or(text);
    #[cfg(not(windows))]
    let disk_text = text;
    if disk_text.starts_with("\\\\")
        || disk_text.starts_with("//")
        || disk_text.starts_with("\\??\\")
    {
        return Err("network and device image paths are not allowed".into());
    }
    if !Path::new(text).is_absolute() {
        return Err("image path must be absolute".into());
    }
    #[cfg(windows)]
    {
        validate_windows_disk_path(text, true)?;
        for component in disk_text[3..].split(['\\', '/']) {
            validate_windows_component(component)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_component(component: &str) -> Result<(), String> {
    if component.is_empty() {
        return Ok(());
    }
    if component.ends_with(['.', ' ']) || component.contains(':') {
        return Err("image path has an ambiguous component or alternate data stream".into());
    }
    if crate::windows_paths::is_device_name(component) {
        return Err("image path contains a Windows device name".into());
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_disk_path(text: &str, allow_verbatim: bool) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDriveTypeW(root_path: *const u16) -> u32;
    }

    let text = if allow_verbatim {
        text.strip_prefix("\\\\?\\").unwrap_or(text)
    } else {
        text
    };
    let bytes = text.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
        || text[2..].contains(':')
    {
        return Err(
            "image path must resolve to an absolute local drive path without streams".into(),
        );
    }
    let root = format!("{}:\\", char::from(bytes[0]));
    let root: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Unknown, missing and remote drives are not accepted as local storage.
    if !matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3 | 5 | 6) {
        return Err("image path is not on a recognized local drive".into());
    }
    Ok(())
}

fn reject_link_components(path: &Path) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        // A Windows drive prefix alone (C:) is not an absolute path yet.
        if !prefix.is_absolute() {
            continue;
        }
        let metadata = fs::symlink_metadata(&prefix)
            .map_err(|error| format!("cannot inspect image path component: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("symlink image paths are not allowed".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err("reparse-point image paths are not allowed".into());
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn validate_open_file_path(file: &File) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileType(file: *mut std::ffi::c_void) -> u32;
        fn GetFinalPathNameByHandleW(
            file: *mut std::ffi::c_void,
            path: *mut u16,
            length: u32,
            flags: u32,
        ) -> u32;
    }

    if unsafe { GetFileType(file.as_raw_handle().cast()) } != 1 {
        return Err("opened image handle is not a disk file".into());
    }
    let mut buffer = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle().cast(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    };
    if length == 0 || length as usize >= buffer.len() {
        return Err("cannot establish the opened image file's local disk path".into());
    }
    let path = String::from_utf16(&buffer[..length as usize])
        .map_err(|_| "opened image path is not valid Unicode".to_string())?;
    validate_windows_disk_path(&path, true)
}

fn read_local_file(text: &str) -> Result<(PathBuf, Vec<u8>), String> {
    validate_path_text(text)?;
    let path = Path::new(text);
    reject_link_components(path)?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve image path: {error}"))?;
    #[cfg(windows)]
    validate_windows_disk_path(&canonical.to_string_lossy(), true)?;
    let metadata =
        fs::metadata(&canonical).map_err(|error| format!("cannot inspect image file: {error}"))?;
    if !metadata.is_file() {
        return Err("image path must name a regular file".into());
    }
    if metadata.len() == 0 || metadata.len() > MAX_INPUT_BYTES {
        return Err(format!(
            "image file must contain between 1 and {MAX_INPUT_BYTES} bytes"
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Reject a replacement final-component symlink instead of following it.
        options.custom_flags(0x0020_0000);
    }
    let file = options
        .open(&canonical)
        .map_err(|error| format!("cannot open image file: {error}"))?;
    #[cfg(windows)]
    validate_open_file_path(&file)?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect opened image file: {error}"))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if opened_metadata.file_attributes() & 0x400 != 0 {
            return Err("opened image is a reparse point".into());
        }
    }
    if !opened_metadata.is_file()
        || opened_metadata.len() == 0
        || opened_metadata.len() > MAX_INPUT_BYTES
    {
        return Err(
            "opened image is not a nonempty regular file within the input size limit".into(),
        );
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read image file: {error}"))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err("image file changed or exceeded the input size limit while reading".into());
    }
    Ok((canonical, bytes))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn parse_hex_record(line: &str) -> Result<(u16, u8, Vec<u8>), String> {
    if !line.starts_with(':') || line.len() < 11 || line.len() > 521 || line.len() % 2 != 1 {
        return Err("record must start with ':' and contain 5..260 encoded bytes".into());
    }
    let digits = &line.as_bytes()[1..];
    if !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err("record contains a non-hexadecimal character".into());
    }
    let bytes: Vec<u8> = digits
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).expect("validated hex digit");
            let low = (pair[1] as char).to_digit(16).expect("validated hex digit");
            ((high << 4) | low) as u8
        })
        .collect();
    if bytes.len() != usize::from(bytes[0]) + 5 {
        return Err("record byte count does not match its encoded length".into());
    }
    if bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        return Err("record checksum is invalid".into());
    }
    Ok((
        u16::from_be_bytes([bytes[1], bytes[2]]),
        bytes[3],
        bytes[4..bytes.len() - 1].to_vec(),
    ))
}

#[derive(Default)]
struct HexRecords {
    base: u64,
    eof: bool,
    decoded_bytes: usize,
    records: Vec<Segment>,
    entry_point: Option<u32>,
}

impl HexRecords {
    fn push_data(
        &mut self,
        offset: u16,
        data: Vec<u8>,
        max_image_bytes: usize,
    ) -> Result<(), String> {
        self.decoded_bytes += data.len();
        if self.decoded_bytes > max_image_bytes {
            return Err("decoded record bytes exceed max_image_bytes".into());
        }
        let address = self.base + u64::from(offset);
        if address >= ADDRESS_SPACE_END || address + data.len() as u64 > ADDRESS_SPACE_END {
            return Err("data address exceeds the 32-bit address space".into());
        }
        // Reject ambiguous segmented wraparound; producers can split
        // explicitly at a 64-KiB boundary with an updated base record.
        if usize::from(offset) + data.len() > 65536 {
            return Err("data record crosses a 64-KiB offset boundary".into());
        }
        if !data.is_empty() {
            self.records.push(Segment {
                address: address as u32,
                bytes: data,
            });
        }
        Ok(())
    }

    fn set_entry_point(&mut self, kind: u8, data: &[u8]) -> Result<(), String> {
        let address = if kind == 3 {
            (u32::from(u16::from_be_bytes([data[0], data[1]])) << 4)
                + u32::from(u16::from_be_bytes([data[2], data[3]]))
        } else {
            u32::from_be_bytes([data[0], data[1], data[2], data[3]])
        };
        if self.entry_point.is_some_and(|previous| previous != address) {
            return Err("conflicting start-address records".into());
        }
        self.entry_point = Some(address);
        Ok(())
    }

    fn push(
        &mut self,
        offset: u16,
        kind: u8,
        data: Vec<u8>,
        max_image_bytes: usize,
    ) -> Result<(), String> {
        match kind {
            0 => return self.push_data(offset, data, max_image_bytes),
            1 if offset == 0 && data.is_empty() => self.eof = true,
            2 | 4 if offset == 0 && data.len() == 2 => {
                let upper = u64::from(u16::from_be_bytes([data[0], data[1]]));
                self.base = upper << if kind == 2 { 4 } else { 16 };
            }
            3 | 5 if offset == 0 && data.len() == 4 => {
                return self.set_entry_point(kind, &data);
            }
            1..=5 => {
                return Err("invalid record length or nonzero metadata-record address".into());
            }
            _ => return Err("unsupported record type".into()),
        }
        Ok(())
    }
}

fn merge_hex_records(mut records: Vec<Segment>) -> Result<Vec<Segment>, String> {
    records.sort_unstable_by_key(|segment| segment.address);
    let mut segments: Vec<Segment> = Vec::new();
    for record in records {
        if let Some(previous) = segments.last_mut()
            && u64::from(record.address) <= previous.end()
        {
            let offset = (record.address - previous.address) as usize;
            let overlap = (previous.bytes.len() - offset).min(record.bytes.len());
            if previous.bytes[offset..offset + overlap] != record.bytes[..overlap] {
                return Err(format!(
                    "Intel HEX contains conflicting overlap at or after 0x{:08X}",
                    record.address
                ));
            }
            previous.bytes.extend_from_slice(&record.bytes[overlap..]);
            continue;
        }
        if segments.len() == MAX_SEGMENTS {
            return Err(format!(
                "Intel HEX must not contain more than {MAX_SEGMENTS} disjoint segments"
            ));
        }
        segments.push(record);
    }
    Ok(segments)
}

fn parse_hex(bytes: &[u8], max_image_bytes: usize) -> Result<(Vec<Segment>, Option<u32>), String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "Intel HEX must contain ASCII hexadecimal records".to_string())?;
    let mut parsed = HexRecords::default();
    for (index, line) in text.split_terminator('\n').enumerate() {
        let line_number = index + 1;
        if line_number > MAX_HEX_RECORDS {
            return Err(format!("Intel HEX exceeds {MAX_HEX_RECORDS} records"));
        }
        if parsed.eof {
            return Err(format!("Intel HEX line {line_number}: record follows EOF"));
        }
        let fail = |error| format!("Intel HEX line {line_number}: {error}");
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (offset, kind, data) = parse_hex_record(line).map_err(fail)?;
        parsed
            .push(offset, kind, data, max_image_bytes)
            .map_err(fail)?;
    }
    if !parsed.eof {
        return Err("Intel HEX is missing its EOF record".into());
    }
    if parsed.records.is_empty() {
        return Err("Intel HEX contains no image bytes".into());
    }
    Ok((merge_hex_records(parsed.records)?, parsed.entry_point))
}

/// Fully validate the request and capture immutable expected bytes offline.
pub fn prepare(request: VerifyImageRequest) -> Result<PreparedImage, String> {
    if !(1..=MAX_CHUNK_BYTES).contains(&request.chunk_bytes) {
        return Err(format!(
            "chunk_bytes must be between 1 and {MAX_CHUNK_BYTES}"
        ));
    }
    if !(1..=MAX_IMAGE_BYTES).contains(&request.max_image_bytes) {
        return Err(format!(
            "max_image_bytes must be between 1 and {MAX_IMAGE_BYTES}"
        ));
    }
    if !(1..=60000).contains(&request.timeout_ms) {
        return Err("timeout_ms must be between 1 and 60000".into());
    }
    if request.max_mismatches > MAX_MISMATCHES {
        return Err(format!("max_mismatches must not exceed {MAX_MISMATCHES}"));
    }
    let allowed_ranges = prepare_allowed_ranges(&request.allowed_ranges)?;
    let base_address = match request.format {
        ImageFormat::Bin => {
            Some(parse_address(request.base_address.as_deref().ok_or_else(
                || "base_address is required for BIN images".to_string(),
            )?)?)
        }
        ImageFormat::IntelHex if request.base_address.is_some() => {
            return Err("base_address must be omitted for Intel HEX images".into());
        }
        ImageFormat::IntelHex => None,
    };
    let (path, bytes) = read_local_file(&request.path)?;
    let file_bytes = bytes.len();
    let input_sha256 = digest(&bytes);
    let (segments, entry_point) = match request.format {
        ImageFormat::Bin => {
            if bytes.len() > request.max_image_bytes {
                return Err("BIN image exceeds max_image_bytes".into());
            }
            let address = base_address.expect("BIN base address was validated");
            if u64::from(address) + bytes.len() as u64 > ADDRESS_SPACE_END {
                return Err("BIN image exceeds the 32-bit address space".into());
            }
            (vec![Segment { address, bytes }], None)
        }
        ImageFormat::IntelHex => parse_hex(&bytes, request.max_image_bytes)?,
    };
    let mut total_bytes = 0usize;
    let mut chunk_count = 0usize;
    let mut payload_hash = Sha256::new();
    for segment in &segments {
        if !allowed_ranges
            .iter()
            .any(|&(start, end)| start <= u64::from(segment.address) && segment.end() <= end)
        {
            return Err(format!(
                "image segment 0x{:08X}..0x{:X} is not fully covered by allowed_ranges",
                segment.address,
                segment.end()
            ));
        }
        total_bytes += segment.bytes.len();
        chunk_count += segment.bytes.len().div_ceil(request.chunk_bytes);
        if chunk_count > MAX_CHUNKS {
            return Err(format!(
                "image requires more than {MAX_CHUNKS} chunks; increase chunk_bytes"
            ));
        }
        payload_hash.update(&segment.bytes);
    }
    Ok(PreparedImage {
        path,
        format: request.format,
        file_bytes,
        input_sha256,
        payload_sha256: format!("{:x}", payload_hash.finalize()),
        segments,
        allowed_ranges,
        entry_point,
        total_bytes,
        chunk_count,
        access: request.access,
        chunk_bytes: request.chunk_bytes,
        timeout_ms: request.timeout_ms,
        max_mismatches: request.max_mismatches,
    })
}

fn bounded_error(text: &str) -> String {
    if text.len() <= MAX_ERROR_BYTES {
        return text.to_string();
    }
    let mut end = MAX_ERROR_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &text[..end])
}

fn stop_reason(code: &str, message: &str, address: u32, segment_index: usize) -> Value {
    json!({
        "code": code,
        "message": bounded_error(message),
        "address": format!("0x{address:08X}"),
        "segment_index": segment_index,
    })
}

/// Verify an already prepared image on the caller's connected thread.
pub fn verify(prepared: PreparedImage) -> Value {
    verify_with(prepared, crate::t32_state, crate::t32_read_memory)
}

/// Injectable backend for offline tests. There are no implicit retries,
/// dual-port fallbacks, writes, connections, or run-control operations.
pub fn verify_with(
    prepared: PreparedImage,
    state: impl FnMut() -> Result<i32, String>,
    read: impl FnMut(u32, usize, i32, bool) -> Result<Vec<u8>, String>,
) -> Value {
    let started = Instant::now();
    verify_with_clock(prepared, state, read, || started.elapsed())
}

struct ImageReader<State, Read, Elapsed> {
    state: State,
    read: Read,
    elapsed: Elapsed,
    deadline: Duration,
    access: MemoryAccess,
    initial_state: Option<i32>,
    last_state: Option<i32>,
    state_check_count: usize,
    attempted_chunks: usize,
    discarded_read_bytes: usize,
}

impl<State, Read, Elapsed> ImageReader<State, Read, Elapsed>
where
    State: FnMut() -> Result<i32, String>,
    Read: FnMut(u32, usize, i32, bool) -> Result<Vec<u8>, String>,
    Elapsed: FnMut() -> Duration,
{
    fn sample_state(&mut self) -> Result<i32, String> {
        self.state_check_count += 1;
        let result = (self.state)();
        self.last_state = result.as_ref().ok().copied();
        result
    }

    fn state_before(&mut self, address: u32, segment_index: usize) -> Result<i32, Value> {
        let fail = |code: &str, message: &str| stop_reason(code, message, address, segment_index);
        if (self.elapsed)() >= self.deadline {
            return Err(fail(
                "deadline_exceeded",
                "soft deadline expired before the next call",
            ));
        }
        let before = self
            .sample_state()
            .map_err(|error| fail("state_unknown", &error))?;
        self.initial_state.get_or_insert(before);
        if !matches!(before, 1 | 2) {
            return Err(fail(
                "target_not_stopped",
                &format!("image verification requires state 1 or 2; observed {before}"),
            ));
        }
        if self.initial_state.is_some_and(|initial| initial != before) {
            return Err(fail(
                "state_changed",
                "target state changed between checked chunks",
            ));
        }
        if (self.elapsed)() >= self.deadline {
            return Err(fail(
                "deadline_exceeded",
                "soft deadline expired after checking target state",
            ));
        }
        Ok(before)
    }

    fn read_chunk(
        &mut self,
        address: u32,
        length: usize,
        segment_index: usize,
    ) -> Result<Vec<u8>, Value> {
        let before = self.state_before(address, segment_index)?;
        let fail = |code: &str, message: &str| stop_reason(code, message, address, segment_index);
        self.attempted_chunks += 1;
        let actual = match (self.read)(address, length, self.access.native(), false) {
            Ok(bytes) if bytes.len() == length => bytes,
            Ok(bytes) => {
                self.last_state = None;
                self.discarded_read_bytes += bytes.len();
                return Err(fail(
                    "invalid_read_length",
                    &format!("read returned {} bytes; expected {length}", bytes.len()),
                ));
            }
            Err(error) => {
                self.last_state = None;
                return Err(fail("read_failed", &error));
            }
        };
        // Coverage accepts a chunk only after a confirming state sample.
        // An expired deadline must not trigger another native call.
        if (self.elapsed)() >= self.deadline {
            self.discarded_read_bytes += actual.len();
            self.last_state = None;
            return Err(fail(
                "deadline_exceeded",
                "soft deadline expired after read; post-read state was not checked",
            ));
        }
        let after = self.sample_state().map_err(|error| {
            self.discarded_read_bytes += actual.len();
            fail("state_unknown", &error)
        })?;
        if after != before {
            self.discarded_read_bytes += actual.len();
            return Err(fail(
                "state_changed",
                &format!("target state changed from {before} to {after} during read"),
            ));
        }
        Ok(actual)
    }
}

fn count_mismatches(
    address: u32,
    actual: &[u8],
    expected: &[u8],
    mismatch_addresses: &mut Vec<String>,
    max_mismatches: usize,
) -> usize {
    let mut mismatches = 0;
    for (offset, (&actual_byte, &expected_byte)) in actual.iter().zip(expected).enumerate() {
        if actual_byte != expected_byte {
            mismatches += 1;
            if mismatch_addresses.len() < max_mismatches {
                mismatch_addresses.push(format!("0x{:08X}", address + offset as u32));
            }
        }
    }
    mismatches
}

fn verify_with_clock(
    prepared: PreparedImage,
    state: impl FnMut() -> Result<i32, String>,
    read: impl FnMut(u32, usize, i32, bool) -> Result<Vec<u8>, String>,
    elapsed: impl FnMut() -> Duration,
) -> Value {
    let started_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut reader = ImageReader {
        state,
        read,
        elapsed,
        deadline: Duration::from_millis(prepared.timeout_ms),
        access: prepared.access,
        initial_state: None,
        last_state: None,
        state_check_count: 0,
        attempted_chunks: 0,
        discarded_read_bytes: 0,
    };
    let mut checked_bytes = 0usize;
    let mut checked_chunks = 0usize;
    let mut mismatch_count = 0usize;
    let mut mismatch_addresses = Vec::new();
    let mut actual_hash = Sha256::new();
    let mut checked_expected_hash = Sha256::new();
    let mut failure = None;
    let mut segment_results = Vec::with_capacity(prepared.segments.len());
    let mut checked_ranges = Vec::new();

    for (segment_index, segment) in prepared.segments.iter().enumerate() {
        let mut segment_checked = 0usize;
        let mut segment_mismatches = 0usize;
        let mut segment_actual_hash = Sha256::new();
        for expected in segment.bytes.chunks(prepared.chunk_bytes) {
            if failure.is_some() {
                break;
            }
            let address = segment.address + segment_checked as u32;
            let actual = match reader.read_chunk(address, expected.len(), segment_index) {
                Ok(bytes) => bytes,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            let mismatches = count_mismatches(
                address,
                &actual,
                expected,
                &mut mismatch_addresses,
                prepared.max_mismatches,
            );
            mismatch_count += mismatches;
            segment_mismatches += mismatches;
            checked_bytes += expected.len();
            checked_chunks += 1;
            segment_checked += expected.len();
            actual_hash.update(&actual);
            checked_expected_hash.update(expected);
            segment_actual_hash.update(&actual);
        }
        let segment_complete = segment_checked == segment.bytes.len();
        let checked_range = (segment_checked > 0).then(
            || json!({"address": format!("0x{:08X}", segment.address), "length": segment_checked}),
        );
        if let Some(range) = checked_range.as_ref() {
            checked_ranges.push(range.clone());
        }
        segment_results.push(json!({
            "address": format!("0x{:08X}", segment.address),
            "length": segment.bytes.len(),
            "expected_sha256": digest(&segment.bytes),
            "checked_bytes": segment_checked,
            "checked_range": checked_range,
            "checked_prefix_sha256": (segment_checked > 0).then(|| format!("{:x}", segment_actual_hash.finalize())),
            "expected_checked_prefix_sha256": (segment_checked > 0).then(|| digest(&segment.bytes[..segment_checked])),
            "complete": segment_complete,
            "matches": segment_complete.then_some(segment_mismatches == 0),
            "mismatch_count": segment_mismatches,
        }));
    }

    let complete = checked_bytes == prepared.total_bytes && failure.is_none();
    let elapsed_ms = (reader.elapsed)().as_millis();
    json!({
        "schema_version": 1,
        "kind": "image_verification",
        "ok": complete,
        "status": if complete { if mismatch_count == 0 { "match" } else { "mismatch" } } else { "incomplete" },
        "complete": complete,
        "matches": complete.then_some(mismatch_count == 0),
        "partial": checked_bytes > 0 && !complete,
        "input": {
            "path": prepared.path,
            "format": prepared.format,
            "file_bytes": prepared.file_bytes,
            "sha256": prepared.input_sha256,
            "entry_point_not_applied": prepared.entry_point.map(|address| format!("0x{address:08X}")),
        },
        "access": prepared.access,
        "dual_port": false,
        "allowed_ranges": prepared.allowed_ranges.iter().map(|&(start,end)| json!({
            "address": format!("0x{start:08X}"), "length": end - start
        })).collect::<Vec<_>>(),
        "expected_bytes": prepared.total_bytes,
        "checked_bytes": checked_bytes,
        "expected_chunks": prepared.chunk_count,
        "attempted_chunks": reader.attempted_chunks,
        "checked_chunks": checked_chunks,
        "chunk_bytes": prepared.chunk_bytes,
        "discarded_read_bytes": reader.discarded_read_bytes,
        "mismatch_count": mismatch_count,
        "mismatch_count_scope": "checked_ranges_only",
        "mismatch_addresses": mismatch_addresses,
        "mismatch_addresses_truncated": mismatch_count > prepared.max_mismatches,
        "checked_ranges": checked_ranges,
        "expected_payload_sha256": prepared.payload_sha256,
        "checked_payload_sha256": (checked_bytes > 0).then(|| format!("{:x}", actual_hash.finalize())),
        "expected_checked_payload_sha256": (checked_bytes > 0).then(|| format!("{:x}", checked_expected_hash.finalize())),
        "payload_hash_scope": "Bytes concatenated in ascending segment-address order; HEX holes are excluded. Checked hashes cover only checked_ranges, not the whole image unless complete.",
        "segments": segment_results,
        "initial_state": reader.initial_state,
        "last_state": reader.last_state,
        "state_check_count": reader.state_check_count,
        "state_policy": "Already halted/stopped only; same state sampled before and after each accepted chunk. Sampling cannot prove the target never ran between checks.",
        "started_unix_ms": started_unix_ms,
        "elapsed_ms": elapsed_ms,
        "timeout_ms": prepared.timeout_ms,
        "deadline_exceeded": elapsed_ms >= u128::from(prepared.timeout_ms),
        "deadline_policy": "Soft deadline checked between native calls; cannot interrupt a blocked call.",
        "stop_reason": failure,
        "authenticity_verified": false,
        "raw_target_bytes_included": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::io::Write;

    fn fixture(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.flush().unwrap();
        file
    }

    fn request(file: &tempfile::NamedTempFile, format: ImageFormat) -> VerifyImageRequest {
        VerifyImageRequest {
            path: file.path().to_string_lossy().into_owned(),
            format,
            base_address: matches!(format, ImageFormat::Bin).then(|| "0x1000".into()),
            allowed_ranges: vec![AllowedAddressRange {
                address: "0".into(),
                length: ADDRESS_SPACE_END,
            }],
            access: MemoryAccess::Data,
            chunk_bytes: 4096,
            max_image_bytes: MAX_IMAGE_BYTES,
            timeout_ms: 60000,
            max_mismatches: 64,
            save_artifact: false,
        }
    }

    fn record(offset: u16, kind: u8, data: &[u8]) -> String {
        assert!(data.len() <= 255);
        let mut bytes = vec![data.len() as u8, (offset >> 8) as u8, offset as u8, kind];
        bytes.extend_from_slice(data);
        bytes.push(0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))));
        format!(
            ":{}\n",
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        )
    }

    fn hex_image(records: &[String]) -> tempfile::NamedTempFile {
        fixture(format!("{}{}", records.concat(), record(0, 1, &[])).as_bytes())
    }

    fn prepared_bin(bytes: &[u8], chunk_bytes: usize) -> PreparedImage {
        let file = fixture(bytes);
        let mut request = request(&file, ImageFormat::Bin);
        request.chunk_bytes = chunk_bytes;
        prepare(request).unwrap()
    }

    #[test]
    fn bin_requires_explicit_base_and_hex_rejects_relocation() {
        let file = fixture(&[1]);
        let mut bin = request(&file, ImageFormat::Bin);
        bin.base_address = None;
        assert!(prepare(bin).unwrap_err().contains("base_address"));
        let mut hex = request(&file, ImageFormat::IntelHex);
        hex.base_address = Some("0".into());
        assert!(prepare(hex).unwrap_err().contains("omitted"));
    }

    #[test]
    fn address_parser_rejects_injection_and_overflow() {
        for invalid in [
            "",
            "0x",
            "-1",
            "+1",
            "1.",
            "0x100000000",
            "4294967296",
            "1;Go",
            " 1",
            "1 ",
        ] {
            assert!(parse_address(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(parse_address("0XFFFFFFFF").unwrap(), u32::MAX);
        assert_eq!(parse_address("4294967295").unwrap(), u32::MAX);
    }

    #[test]
    fn bin_allows_last_address_but_never_wraps() {
        let file = fixture(&[1]);
        let mut at_end = request(&file, ImageFormat::Bin);
        at_end.base_address = Some("0xffffffff".into());
        assert_eq!(prepare(at_end).unwrap().total_bytes, 1);
        let file = fixture(&[1, 2]);
        let mut overflow = request(&file, ImageFormat::Bin);
        overflow.base_address = Some("0xffffffff".into());
        assert!(prepare(overflow).unwrap_err().contains("address space"));
    }

    #[test]
    fn bin_input_and_payload_hash_match_exact_captured_bytes() {
        let bytes = [1, 2, 3, 4];
        let prepared = prepared_bin(&bytes, 2);
        assert_eq!(prepared.input_sha256, digest(&bytes));
        assert_eq!(prepared.payload_sha256, digest(&bytes));
        assert_eq!(prepared.chunk_count, 2);
    }

    #[test]
    fn prepared_image_does_not_reopen_changed_input() {
        let mut file = fixture(&[1, 2]);
        let prepared = prepare(request(&file, ImageFormat::Bin)).unwrap();
        file.as_file_mut().set_len(0).unwrap();
        let result = verify_with(prepared, || Ok(1), |_, _, _, _| Ok(vec![1, 2]));
        assert_eq!(result["matches"], true);
        assert_eq!(result["input"]["sha256"], digest(&[1, 2]));
    }

    #[test]
    fn allowed_ranges_are_required_and_all_bytes_must_be_covered() {
        let file = fixture(&[1, 2]);
        let mut empty = request(&file, ImageFormat::Bin);
        empty.allowed_ranges.clear();
        assert!(prepare(empty).unwrap_err().contains("allowed_ranges"));
        let mut partial = request(&file, ImageFormat::Bin);
        partial.allowed_ranges = vec![AllowedAddressRange {
            address: "0x1000".into(),
            length: 1,
        }];
        assert!(prepare(partial).unwrap_err().contains("not fully covered"));
    }

    #[test]
    fn adjacent_allowed_ranges_merge_but_holes_are_not_allowed() {
        let file = fixture(&[1, 2, 3]);
        let mut adjacent = request(&file, ImageFormat::Bin);
        adjacent.allowed_ranges = vec![
            AllowedAddressRange {
                address: "0x1002".into(),
                length: 1,
            },
            AllowedAddressRange {
                address: "0x1000".into(),
                length: 2,
            },
        ];
        assert!(prepare(adjacent).is_ok());
        let mut hole = request(&file, ImageFormat::Bin);
        hole.allowed_ranges = vec![
            AllowedAddressRange {
                address: "0x1000".into(),
                length: 1,
            },
            AllowedAddressRange {
                address: "0x1002".into(),
                length: 1,
            },
        ];
        assert!(prepare(hole).is_err());
    }

    #[test]
    fn allowed_ranges_reject_zero_length_overflow_and_excess_count() {
        for (address, length) in [("0", 0), ("0xffffffff", 2), ("1", u64::MAX)] {
            assert!(
                prepare_allowed_ranges(&[AllowedAddressRange {
                    address: address.into(),
                    length
                }])
                .is_err()
            );
        }
        let ranges = (0..65)
            .map(|_| AllowedAddressRange {
                address: "0".into(),
                length: 1,
            })
            .collect::<Vec<_>>();
        assert!(prepare_allowed_ranges(&ranges).is_err());
    }

    #[test]
    fn request_caps_are_validated_before_opening_a_file() {
        let file = fixture(&[1]);
        for cap in [0, MAX_CHUNK_BYTES + 1] {
            let mut value = request(&file, ImageFormat::Bin);
            value.path = "missing".into();
            value.chunk_bytes = cap;
            assert!(prepare(value).unwrap_err().contains("chunk_bytes"));
        }
        for cap in [0, MAX_IMAGE_BYTES + 1] {
            let mut value = request(&file, ImageFormat::Bin);
            value.max_image_bytes = cap;
            assert!(prepare(value).unwrap_err().contains("max_image_bytes"));
        }
        for timeout in [0, 60001] {
            let mut value = request(&file, ImageFormat::Bin);
            value.timeout_ms = timeout;
            assert!(prepare(value).unwrap_err().contains("timeout_ms"));
        }
        let mut value = request(&file, ImageFormat::Bin);
        value.max_mismatches = MAX_MISMATCHES + 1;
        assert!(prepare(value).unwrap_err().contains("max_mismatches"));
    }

    #[test]
    fn image_and_chunk_caps_are_enforced_offline() {
        let file = fixture(&[1, 2, 3]);
        let mut value = request(&file, ImageFormat::Bin);
        value.max_image_bytes = 2;
        assert!(prepare(value).unwrap_err().contains("max_image_bytes"));
        let file = fixture(&vec![0; MAX_CHUNKS + 1]);
        let mut value = request(&file, ImageFormat::Bin);
        value.chunk_bytes = 1;
        assert!(prepare(value).unwrap_err().contains("chunks"));
    }

    #[test]
    fn paths_reject_relative_network_device_and_control_text() {
        for path in [
            "image.bin",
            r"\\host\share\image.bin",
            "//host/share/image.bin",
            r"\\.\NUL",
            r"\??\C:\image.bin",
            "bad\nfile",
        ] {
            assert!(validate_path_text(path).is_err(), "{path:?}");
        }
        assert!(validate_path_text(&"x".repeat(4097)).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_reject_streams_reserved_names_and_ambiguous_components() {
        let drive = std::env::temp_dir().to_string_lossy()[..2].to_string();
        for suffix in [
            r"\NUL.bin",
            r"\AUX",
            r"\com1.txt",
            r"\LPT9",
            r"\file.bin:stream",
            r"\name.\file.bin",
            r"\name \file.bin",
            r"\..\file.bin",
        ] {
            assert!(
                validate_path_text(&format!("{drive}{suffix}")).is_err(),
                "{suffix}"
            );
        }
        assert!(validate_windows_disk_path(r"\\?\UNC\host\share\file.bin", true).is_err());
    }

    #[test]
    fn empty_directory_and_oversized_files_are_rejected() {
        let file = fixture(&[]);
        assert!(prepare(request(&file, ImageFormat::Bin)).is_err());
        let mut directory = request(&file, ImageFormat::Bin);
        directory.path = file.path().parent().unwrap().to_string_lossy().into_owned();
        assert!(prepare(directory).unwrap_err().contains("regular file"));
        file.as_file().set_len(MAX_INPUT_BYTES + 1).unwrap();
        assert!(
            prepare(request(&file, ImageFormat::Bin))
                .unwrap_err()
                .contains("bytes")
        );
    }

    #[test]
    fn hex_linear_addresses_sparse_holes_and_entry_point_are_preserved() {
        let file = hex_image(&[
            record(0, 4, &[0x08, 0x00]),
            record(0x10, 0, &[1, 2]),
            record(0x20, 0, &[3]),
            record(0, 5, &[0x08, 0x00, 0x00, 0x11]),
        ]);
        let prepared = prepare(request(&file, ImageFormat::IntelHex)).unwrap();
        assert_eq!(prepared.segments.len(), 2);
        assert_eq!(prepared.entry_point, Some(0x08000011));
        assert_eq!(prepared.payload_sha256, digest(&[1, 2, 3]));
        let mut reads = Vec::new();
        let result = verify_with(
            prepared,
            || Ok(2),
            |address, length, _, dual| {
                reads.push((address, length, dual));
                Ok(if address == 0x08000010 {
                    vec![1, 2]
                } else {
                    vec![3]
                })
            },
        );
        assert_eq!(reads, vec![(0x08000010, 2, false), (0x08000020, 1, false)]);
        assert_eq!(result["matches"], true);
        assert_eq!(result["checked_ranges"].as_array().unwrap().len(), 2);
        assert_eq!(result["checked_payload_sha256"], digest(&[1, 2, 3]));
    }

    #[test]
    fn hex_segment_address_and_start_segment_records_are_supported() {
        let file = hex_image(&[
            record(0, 2, &[0x12, 0x34]),
            record(0x10, 0, &[7]),
            record(0, 3, &[0x12, 0x34, 0, 0x10]),
        ]);
        let prepared = prepare(request(&file, ImageFormat::IntelHex)).unwrap();
        assert_eq!(prepared.segments[0].address, 0x12350);
        assert_eq!(prepared.entry_point, Some(0x12350));
    }

    #[test]
    fn hex_metadata_does_not_fill_an_address_gap() {
        let file = hex_image(&[record(0, 0, &[1]), record(65535, 0, &[2])]);
        let prepared = prepare(request(&file, ImageFormat::IntelHex)).unwrap();
        assert_eq!(prepared.total_bytes, 2);
        assert_eq!(prepared.segments.len(), 2);
    }

    #[test]
    fn hex_order_identical_overlap_and_adjacent_records_merge() {
        let file = hex_image(&[
            record(2, 0, &[3, 4]),
            record(0, 0, &[1, 2, 3]),
            record(1, 0, &[2]),
            record(4, 0, &[5]),
        ]);
        let prepared = prepare(request(&file, ImageFormat::IntelHex)).unwrap();
        assert_eq!(prepared.segments.len(), 1);
        assert_eq!(prepared.segments[0].bytes, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn conflicting_overlap_is_rejected() {
        let file = hex_image(&[record(0, 0, &[1, 2]), record(1, 0, &[9])]);
        assert!(
            prepare(request(&file, ImageFormat::IntelHex))
                .unwrap_err()
                .contains("overlap")
        );
    }

    #[test]
    fn hex_record_validation_precedes_segment_merging() {
        let overlap = format!("{}{}", record(0, 0, &[1]), record(0, 0, &[2]));
        let invalid = format!("{}{}{}", overlap, record(1, 4, &[0, 1]), record(0, 1, &[]));
        assert_eq!(
            parse_hex(invalid.as_bytes(), 1024).unwrap_err(),
            "Intel HEX line 3: invalid record length or nonzero metadata-record address"
        );
        assert_eq!(
            parse_hex(overlap.as_bytes(), 1024).unwrap_err(),
            "Intel HEX is missing its EOF record"
        );
        let complete = format!("{}{}", overlap, record(0, 1, &[]));
        assert_eq!(
            parse_hex(complete.as_bytes(), 1).unwrap_err(),
            "Intel HEX line 2: decoded record bytes exceed max_image_bytes"
        );
        assert_eq!(
            parse_hex(format!("{complete}not a record\n").as_bytes(), 1024).unwrap_err(),
            "Intel HEX line 4: record follows EOF"
        );
    }

    #[test]
    fn hex_last_address_is_allowed_and_overflow_is_rejected() {
        let file = hex_image(&[record(0, 4, &[0xff, 0xff]), record(0xffff, 0, &[1])]);
        assert_eq!(
            prepare(request(&file, ImageFormat::IntelHex))
                .unwrap()
                .segments[0]
                .address,
            u32::MAX
        );
        let file = hex_image(&[record(0, 4, &[0xff, 0xff]), record(0xffff, 0, &[1, 2])]);
        assert!(
            prepare(request(&file, ImageFormat::IntelHex))
                .unwrap_err()
                .contains("address space")
        );
        let file = hex_image(&[record(0xffff, 0, &[1, 2])]);
        assert!(
            prepare(request(&file, ImageFormat::IntelHex))
                .unwrap_err()
                .contains("64-KiB")
        );
    }

    #[test]
    fn malformed_record_text_count_and_checksum_are_rejected() {
        for line in [
            "",
            ":",
            ":00000001",
            ":00000001FF00",
            ":00000001FG",
            ":00000001FE",
            ":01000001FF",
            " :00000001FF",
            ":00000001FF ",
        ] {
            assert!(parse_hex_record(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn hex_requires_one_final_eof_and_some_data() {
        for bytes in [
            record(0, 0, &[1]),
            record(0, 1, &[]),
            format!(
                "{}{}{}",
                record(0, 0, &[1]),
                record(0, 1, &[]),
                record(0, 1, &[])
            ),
            format!("{}{}", record(0, 1, &[]), record(0, 0, &[1])),
            format!("{}\n{}", record(0, 0, &[1]), record(0, 1, &[])),
        ] {
            assert!(parse_hex(bytes.as_bytes(), 1024).is_err());
        }
    }

    #[test]
    fn hex_crlf_and_final_record_without_newline_are_accepted() {
        let text = format!("{}{}", record(0, 0, &[1]), record(0, 1, &[]));
        assert!(parse_hex(text.replace('\n', "\r\n").as_bytes(), 1024).is_ok());
        assert!(parse_hex(text.trim_end().as_bytes(), 1024).is_ok());
    }

    #[test]
    fn hex_rejects_invalid_metadata_shapes_unknown_types_and_conflicting_entry_points() {
        for invalid in [
            record(1, 1, &[]),
            record(0, 1, &[1]),
            record(0, 2, &[1]),
            record(1, 4, &[0, 1]),
            record(0, 3, &[1, 2, 3]),
            record(1, 5, &[0, 0, 0, 0]),
            record(0, 6, &[]),
        ] {
            assert!(
                parse_hex(
                    format!("{}{}{}", record(0, 0, &[1]), invalid, record(0, 1, &[])).as_bytes(),
                    1024
                )
                .is_err()
            );
        }
        let file = hex_image(&[
            record(0, 0, &[1]),
            record(0, 5, &[0, 0, 0, 1]),
            record(0, 5, &[0, 0, 0, 2]),
        ]);
        assert!(
            prepare(request(&file, ImageFormat::IntelHex))
                .unwrap_err()
                .contains("conflicting start")
        );
    }

    #[test]
    fn hex_record_bytes_and_disjoint_segment_caps_are_enforced() {
        let file = hex_image(&[record(0, 0, &[1, 2]), record(0, 0, &[1, 2])]);
        let mut value = request(&file, ImageFormat::IntelHex);
        value.max_image_bytes = 3;
        assert!(prepare(value).unwrap_err().contains("decoded record bytes"));
        let records: Vec<String> = (0..=MAX_SEGMENTS)
            .map(|index| record((index * 2) as u16, 0, &[1]))
            .collect();
        let file = hex_image(&records);
        assert!(
            prepare(request(&file, ImageFormat::IntelHex))
                .unwrap_err()
                .contains("disjoint segments")
        );
    }

    #[test]
    fn complete_match_has_exact_coverage_and_no_raw_byte_values() {
        let result = verify_with(
            prepared_bin(&[0x12, 0x34, 0x56], 2),
            || Ok(1),
            |address, length, access, dual| {
                assert_eq!(access, crate::ffi::T32_MEMORY_ACCESS_DATA as i32);
                assert!(!dual);
                let offset = (address - 0x1000) as usize;
                Ok([0x12, 0x34, 0x56][offset..offset + length].to_vec())
            },
        );
        assert_eq!(result["complete"], true);
        assert_eq!(result["status"], "match");
        assert_eq!(result["matches"], true);
        assert_eq!(result["state_check_count"], 4);
        assert_eq!(result["checked_bytes"], 3);
        assert_eq!(
            result["checked_payload_sha256"],
            digest(&[0x12, 0x34, 0x56])
        );
        assert_eq!(result["raw_target_bytes_included"], false);
        assert_eq!(result["authenticity_verified"], false);
        assert!(result.get("hex").is_none());
        assert!(result["segments"][0].get("bytes").is_none());
    }

    #[test]
    fn chunk_acquisition_keeps_native_and_clock_call_order() {
        let calls = RefCell::new(Vec::new());
        let result = verify_with_clock(
            prepared_bin(&[1, 2], 1),
            || {
                calls.borrow_mut().push("state");
                Ok(1)
            },
            |address, _, _, _| {
                calls.borrow_mut().push("read");
                Ok(vec![(address - 0x1000 + 1) as u8])
            },
            || {
                calls.borrow_mut().push("elapsed");
                Duration::ZERO
            },
        );
        assert_eq!(
            *calls.borrow(),
            [
                "elapsed", "state", "elapsed", "read", "elapsed", "state", "elapsed", "state",
                "elapsed", "read", "elapsed", "state", "elapsed",
            ]
        );
        assert_eq!(result["matches"], true);
        assert_eq!(result["state_check_count"], 4);
        assert_eq!(result["attempted_chunks"], 2);
        assert_eq!(result["checked_chunks"], 2);
    }

    #[test]
    fn complete_mismatch_counts_all_bytes_but_bounds_addresses() {
        let mut prepared = prepared_bin(&[1, 2, 3, 4], 2);
        prepared.max_mismatches = 1;
        let result = verify_with(prepared, || Ok(2), |_, length, _, _| Ok(vec![0; length]));
        assert_eq!(result["complete"], true);
        assert_eq!(result["matches"], false);
        assert_eq!(result["status"], "mismatch");
        assert_eq!(result["mismatch_count"], 4);
        assert_eq!(result["mismatch_addresses"], json!(["0x00001000"]));
        assert_eq!(result["mismatch_addresses_truncated"], true);
        assert_eq!(result["checked_payload_sha256"], digest(&[0; 4]));
    }

    #[test]
    fn mismatch_address_capture_can_be_disabled() {
        let mut prepared = prepared_bin(&[1], 1);
        prepared.max_mismatches = 0;
        let result = verify_with(prepared, || Ok(1), |_, _, _, _| Ok(vec![2]));
        assert_eq!(result["mismatch_count"], 1);
        assert_eq!(result["mismatch_addresses"], json!([]));
        assert_eq!(result["mismatch_addresses_truncated"], true);
    }

    #[test]
    fn incomplete_hashes_cover_only_checked_prefix_never_whole_image() {
        let result = verify_with(
            prepared_bin(&[1, 2, 3, 4], 2),
            || Ok(1),
            |address, _, _, _| {
                if address == 0x1000 {
                    Ok(vec![1, 2])
                } else {
                    Err("native error 6".into())
                }
            },
        );
        assert_eq!(result["complete"], false);
        assert_eq!(result["partial"], true);
        assert_eq!(result["matches"], Value::Null);
        assert_eq!(result["checked_bytes"], 2);
        assert_eq!(
            result["checked_ranges"],
            json!([{"address": "0x00001000", "length": 2}])
        );
        assert_eq!(result["checked_payload_sha256"], digest(&[1, 2]));
        assert_ne!(
            result["checked_payload_sha256"],
            result["expected_payload_sha256"]
        );
        assert_eq!(
            result["segments"][0]["checked_prefix_sha256"],
            digest(&[1, 2])
        );
        assert_eq!(result["segments"][0]["matches"], Value::Null);
        assert_eq!(result["stop_reason"]["code"], "read_failed");
        assert_eq!(result["stop_reason"]["address"], "0x00001002");
    }

    #[test]
    fn initial_running_down_or_unknown_states_never_read_memory() {
        for value in [0, 3, 4, -1, i32::MAX] {
            let result = verify_with(
                prepared_bin(&[1], 1),
                || Ok(value),
                |_, _, _, _| panic!("unexpected read"),
            );
            assert_eq!(result["checked_bytes"], 0);
            assert_eq!(result["initial_state"], value);
            assert_eq!(result["matches"], Value::Null);
            assert_eq!(result["partial"], false);
            assert_eq!(result["stop_reason"]["code"], "target_not_stopped");
        }
    }

    #[test]
    fn initial_state_error_becomes_structured_incomplete_result() {
        let result = verify_with(
            prepared_bin(&[1], 1),
            || Err("connection unavailable".into()),
            |_, _, _, _| panic!("unexpected read"),
        );
        assert_eq!(result["stop_reason"]["code"], "state_unknown");
        assert_eq!(result["checked_payload_sha256"], Value::Null);
        assert_eq!(result["last_state"], Value::Null);
        assert_eq!(result["complete"], false);
    }

    #[test]
    fn post_read_state_change_discards_chunk_and_reports_incomplete() {
        let mut states = [Ok(1), Ok(3)].into_iter();
        let result = verify_with(
            prepared_bin(&[1, 2], 2),
            || states.next().unwrap(),
            |_, _, _, _| Ok(vec![1, 2]),
        );
        assert_eq!(result["checked_bytes"], 0);
        assert_eq!(result["discarded_read_bytes"], 2);
        assert_eq!(result["matches"], Value::Null);
        assert_eq!(result["stop_reason"]["code"], "state_changed");
        assert_eq!(result["last_state"], 3);
    }

    #[test]
    fn post_read_state_error_discards_chunk() {
        let mut states = [Ok(2), Err("connection lost".into())].into_iter();
        let result = verify_with(
            prepared_bin(&[1], 1),
            || states.next().unwrap(),
            |_, _, _, _| Ok(vec![1]),
        );
        assert_eq!(result["checked_bytes"], 0);
        assert_eq!(result["discarded_read_bytes"], 1);
        assert_eq!(result["stop_reason"]["code"], "state_unknown");
        assert_eq!(result["last_state"], Value::Null);
    }

    #[test]
    fn state_change_between_chunks_stops_before_next_read() {
        let mut states = [Ok(1), Ok(1), Ok(2)].into_iter();
        let reads = Cell::new(0);
        let result = verify_with(
            prepared_bin(&[1, 2], 1),
            || states.next().unwrap(),
            |_, _, _, _| {
                reads.set(reads.get() + 1);
                Ok(vec![1])
            },
        );
        assert_eq!(reads.get(), 1);
        assert_eq!(result["checked_bytes"], 1);
        assert_eq!(result["stop_reason"]["code"], "state_changed");
    }

    #[test]
    fn malformed_read_lengths_are_not_counted_as_coverage() {
        for bytes in [vec![], vec![1, 2, 3]] {
            let result = verify_with(
                prepared_bin(&[1, 2], 2),
                || Ok(1),
                |_, _, _, _| Ok(bytes.clone()),
            );
            assert_eq!(result["checked_bytes"], 0);
            assert_eq!(result["discarded_read_bytes"], bytes.len());
            assert_eq!(result["stop_reason"]["code"], "invalid_read_length");
        }
    }

    #[test]
    fn error_text_is_bounded_on_utf8_boundary() {
        let result = verify_with(
            prepared_bin(&[1], 1),
            || Err("\u{754c}".repeat(3000)),
            |_, _, _, _| panic!("unexpected read"),
        );
        let error = result["stop_reason"]["message"].as_str().unwrap();
        assert!(error.len() <= MAX_ERROR_BYTES + 20);
        assert!(error.ends_with("[truncated]"));
    }

    #[test]
    fn all_access_modes_are_explicit_and_never_use_dual_port() {
        for (access, native) in [
            (MemoryAccess::Data, 0),
            (MemoryAccess::Program, 1),
            (MemoryAccess::ArmDap, 14),
        ] {
            let mut prepared = prepared_bin(&[1], 1);
            prepared.access = access;
            let result = verify_with(
                prepared,
                || Ok(1),
                |_, _, actual, dual| {
                    assert_eq!(actual, native);
                    assert!(!dual);
                    Ok(vec![1])
                },
            );
            assert_eq!(result["matches"], true);
        }
    }

    #[test]
    fn deadline_before_first_call_avoids_backend_entirely() {
        let mut prepared = prepared_bin(&[1], 1);
        prepared.timeout_ms = 1;
        let result = verify_with_clock(
            prepared,
            || panic!("unexpected state"),
            |_, _, _, _| panic!("unexpected read"),
            || Duration::from_millis(1),
        );
        assert_eq!(result["stop_reason"]["code"], "deadline_exceeded");
        assert_eq!(result["attempted_chunks"], 0);
    }

    #[test]
    fn deadline_after_state_call_avoids_memory_read() {
        let mut prepared = prepared_bin(&[1], 1);
        prepared.timeout_ms = 1;
        let clock = Cell::new(0u64);
        let result = verify_with_clock(
            prepared,
            || {
                clock.set(1);
                Ok(1)
            },
            |_, _, _, _| panic!("unexpected read"),
            || Duration::from_millis(clock.get()),
        );
        assert_eq!(result["stop_reason"]["code"], "deadline_exceeded");
        assert_eq!(result["attempted_chunks"], 0);
    }

    #[test]
    fn deadline_after_read_discards_unconfirmed_bytes_without_another_call() {
        let mut prepared = prepared_bin(&[1], 1);
        prepared.timeout_ms = 1;
        let clock = Cell::new(0u64);
        let states = Cell::new(0);
        let result = verify_with_clock(
            prepared,
            || {
                states.set(states.get() + 1);
                Ok(1)
            },
            |_, _, _, _| {
                clock.set(1);
                Ok(vec![1])
            },
            || Duration::from_millis(clock.get()),
        );
        assert_eq!(states.get(), 1);
        assert_eq!(result["checked_bytes"], 0);
        assert_eq!(result["discarded_read_bytes"], 1);
        assert_eq!(result["last_state"], Value::Null);
        assert_eq!(result["stop_reason"]["code"], "deadline_exceeded");
    }

    #[test]
    fn completed_last_state_call_can_exceed_soft_deadline_without_losing_evidence() {
        let mut prepared = prepared_bin(&[1], 1);
        prepared.timeout_ms = 1;
        let clock = Cell::new(0u64);
        let states = Cell::new(0);
        let result = verify_with_clock(
            prepared,
            || {
                states.set(states.get() + 1);
                if states.get() == 2 {
                    clock.set(2);
                }
                Ok(1)
            },
            |_, _, _, _| Ok(vec![1]),
            || Duration::from_millis(clock.get()),
        );
        assert_eq!(result["complete"], true);
        assert_eq!(result["matches"], true);
        assert_eq!(result["deadline_exceeded"], true);
    }

    #[test]
    fn confirmed_chunk_is_retained_when_deadline_prevents_next_chunk() {
        let mut prepared = prepared_bin(&[1, 2], 1);
        prepared.timeout_ms = 1;
        let clock = Cell::new(0u64);
        let states = Cell::new(0);
        let result = verify_with_clock(
            prepared,
            || {
                states.set(states.get() + 1);
                if states.get() == 2 {
                    clock.set(2);
                }
                Ok(1)
            },
            |address, _, _, _| {
                assert_eq!(address, 0x1000);
                Ok(vec![9])
            },
            || Duration::from_millis(clock.get()),
        );
        assert_eq!(result["complete"], false);
        assert_eq!(result["partial"], true);
        assert_eq!(result["state_check_count"], 2);
        assert_eq!(result["attempted_chunks"], 1);
        assert_eq!(result["checked_chunks"], 1);
        assert_eq!(result["checked_bytes"], 1);
        assert_eq!(result["discarded_read_bytes"], 0);
        assert_eq!(result["initial_state"], 1);
        assert_eq!(result["last_state"], 1);
        assert_eq!(result["mismatch_count"], 1);
        assert_eq!(result["mismatch_addresses"], json!(["0x00001000"]));
        assert_eq!(result["checked_payload_sha256"], digest(&[9]));
        assert_eq!(result["expected_checked_payload_sha256"], digest(&[1]));
        assert_eq!(result["stop_reason"]["code"], "deadline_exceeded");
        assert_eq!(result["stop_reason"]["address"], "0x00001001");
        assert_eq!(
            result["stop_reason"]["message"],
            "soft deadline expired before the next call"
        );
    }

    #[test]
    fn sparse_partial_result_keeps_completed_and_unchecked_segments_separate() {
        let file = hex_image(&[record(0, 0, &[1]), record(10, 0, &[2]), record(20, 0, &[3])]);
        let result = verify_with(
            prepare(request(&file, ImageFormat::IntelHex)).unwrap(),
            || Ok(1),
            |address, _, _, _| {
                if address == 0 {
                    Ok(vec![1])
                } else {
                    Err("read failed".into())
                }
            },
        );
        assert_eq!(result["checked_bytes"], 1);
        assert_eq!(result["segments"][0]["complete"], true);
        assert_eq!(result["segments"][0]["matches"], true);
        assert_eq!(result["segments"][1]["complete"], false);
        assert_eq!(result["segments"][1]["matches"], Value::Null);
        assert_eq!(result["segments"][2]["checked_bytes"], 0);
        assert_eq!(result["segments"][2]["checked_prefix_sha256"], Value::Null);
        assert_eq!(result["matches"], Value::Null);
    }

    #[test]
    fn serde_defaults_and_unknown_fields_are_checked() {
        let value = json!({
            "path": "C:\\image.bin", "format": "bin", "base_address": "0",
            "allowed_ranges": [{"address": "0", "length": 1}]
        });
        let parsed: VerifyImageRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(parsed.chunk_bytes, 4096);
        assert_eq!(parsed.max_image_bytes, 1024 * 1024);
        assert_eq!(parsed.timeout_ms, 30000);
        assert!(!parsed.save_artifact);
        let mut invalid = value;
        invalid["dual_port"] = json!(true);
        assert!(serde_json::from_value::<VerifyImageRequest>(invalid).is_err());
    }
}
