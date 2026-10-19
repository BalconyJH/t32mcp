// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Communicating with TRACE32 from Rust. Built on top of Remote API.
//!
//! Pipe communication:
//! - Windows: Named Pipe
//! - Linux: FIFO

use anyhow::Result;
use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::raw::{c_char, c_void};
use std::ptr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

pub mod artifacts;
pub mod catalog;
pub mod inspection;
pub mod peripheral_views;
pub mod peripherals;
pub mod snapshot;

#[cfg(any(windows, test))]
mod windows_paths;

// Vendor builds can use global or thread-local state. Serialize native calls
// and select this thread's channel explicitly for either configuration.
static API_LOCK: Mutex<()> = Mutex::new(());
thread_local! {
    static CHANNEL: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
}

fn native_lock() -> MutexGuard<'static, ()> {
    API_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn connected_api() -> Result<MutexGuard<'static, ()>, String> {
    let guard = native_lock();
    let channel = CHANNEL.get();
    if channel.is_null() {
        return Err(
            "No TRACE32 connection on this thread; connect before querying the native API".into(),
        );
    }
    unsafe { t32::T32_SetChannel(channel) };
    Ok(guard)
}

#[allow(warnings)]
pub mod t32 {
    include!(concat!(env!("OUT_DIR"), "/t32.rs"));
}

#[cfg(target_os = "windows")]
use interprocess::os::windows::named_pipe::{PipeListenerOptions, pipe_mode, tokio::PipeStream};

#[cfg(not(target_os = "windows"))]
use {interprocess::os::unix::fifo_file, tokio::net::unix::pipe, tokio::net::unix::pipe::Receiver};

#[cfg(target_os = "windows")]
pub async fn t32_open_pipe(
    name: &str,
) -> Result<PipeStream<pipe_mode::Bytes, pipe_mode::None>, String> {
    let path = format!(r"\\.\pipe\{name}");

    let listener = PipeListenerOptions::new()
        .path(path)
        .create_tokio_recv_only::<pipe_mode::Bytes>()
        .map_err(|e| format!("Cannot create named pipe: {e}"))?;

    let area = t32_fnc(r"AREA.SELECTed()".to_string())?;
    t32_cmd(format!("AREA.OPEN {area}"))?;
    let name = format!("\\\\.\\pipe\\{name}");
    t32_cmd(format!(r"AREA.PIPE {} {}", area, name))?;

    let connection = match listener.accept().await {
        Ok(c) => c,
        Err(e) => {
            return Err(format!("Cannot setup named pipe: {e}"));
        }
    };

    Ok(connection)
}

#[cfg(not(target_os = "windows"))]
pub async fn t32_open_pipe(name: &str) -> Result<Receiver, String> {
    let fifo_name = format!("/tmp/{name}");
    // Remove a stale FIFO left over from a previous run before re-creating it.
    let _ = std::fs::remove_file(&fifo_name);
    if let Err(e) = fifo_file::create_fifo(&fifo_name, 0o777) {
        return Err(format!("FIFO cannot be created: {e}"));
    }

    let area = t32_fnc(r"AREA.SELECTed()".to_string())?;
    t32_cmd(format!("AREA.OPEN {area}"))?;
    t32_cmd(format!(r"AREA.PIPE {} {}", area, &fifo_name))?;

    let connection = match pipe::OpenOptions::new().open_receiver(&fifo_name) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&fifo_name);
            return Err(format!("Cannot connect to FIFO: {e}"));
        }
    };

    // The receiver holds an open fd; unlink the path so it is cleaned up on close.
    let _ = std::fs::remove_file(&fifo_name);

    Ok(connection)
}

pub fn t32_connect(port: u16) -> Result<(), String> {
    if port == 0 {
        return Err("TRACE32 TCP port must be between 1 and 65535".to_string());
    }
    t32_disconnect();
    // Fail quickly when PowerView is absent. The vendor TCP implementation
    // ignores TIMEOUT=; this is a connection preflight, not an API-call timeout.
    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_secs(1),
    )
    .map_err(|e| {
        format!(
            "Cannot reach TRACE32 at 127.0.0.1:{port}: {e}. Start PowerView with \
             RCL=NETTCP and the matching PORT setting; a powered board alone is not enough."
        )
    })?;
    let guard = native_lock();
    let mut channel: *mut c_void = ptr::null_mut();

    let e = unsafe { t32::T32_RequestChannelNetTcp(&mut channel as *mut *mut c_void) };
    if e != 0 {
        return Err(format!(
            "Connection with TRACE32 failed: T32_RequestChannelNetTcp returned {e}"
        ));
    }

    unsafe {
        t32::T32_SetChannel(channel);
    }
    CHANNEL.set(channel);

    let result = (|| {
        for (key, value) in [
            ("NODE=", "127.0.0.1".to_string()),
            ("PORT=", port.to_string()),
        ] {
            let key = CString::new(key).map_err(|e| e.to_string())?;
            let value = CString::new(value).map_err(|e| e.to_string())?;
            check_api("T32_Config", unsafe {
                t32::T32_Config(key.as_ptr(), value.as_ptr())
            })?;
        }
        check_api("T32_Init", unsafe { t32::T32_Init() })?;
        check_api("T32_Attach", unsafe {
            t32::T32_Attach(t32::T32_DEV_ICD as i32)
        })
    })();
    drop(guard);
    if result.is_err() {
        t32_disconnect();
    }
    result
}

pub fn t32_disconnect() {
    let _guard = native_lock();
    CHANNEL.with(|stored| {
        let mut channel = stored.replace(ptr::null_mut());
        if !channel.is_null() {
            unsafe {
                t32::T32_SetChannel(channel);
                t32::T32_Exit();
                t32::T32_ReleaseChannel(&mut channel);
            }
        }
    });
}

fn check_api(operation: &str, code: i32) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{operation} failed (Remote API error {code})"))
    }
}

fn api_string(value: &str, label: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| format!("{label} must not contain NUL bytes"))
}

fn decode_buffer(buffer: &[u8]) -> Result<String, String> {
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("TRACE32 response exceeded the bounded string buffer")?;
    Ok(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

pub fn t32_cmd(cmd: String) -> Result<(), String> {
    let c = api_string(&cmd, "Command")?;
    let _guard = connected_api()?;
    let e = unsafe { t32::T32_Cmd(c.as_ptr()) };
    check_api("T32_Cmd", e)
}

pub fn t32_fnc(fnc: String) -> Result<String, String> {
    Ok(t32_eval(&fnc)?.value)
}

#[derive(Debug, serde::Serialize)]
pub struct Evaluation {
    pub value: String,
    pub result_type: u32,
}

pub fn t32_eval(expression: &str) -> Result<Evaluation, String> {
    let expression = api_string(expression, "Expression")?;
    let _guard = connected_api()?;
    let buffer_size: u32 = 16 * 1024;
    let mut buffer: Vec<u8> = vec![0u8; buffer_size as usize];
    let mut result_type: u32 = 0;

    let e = unsafe {
        t32::T32_ExecuteFunction(
            expression.as_ptr(),
            buffer.as_mut_ptr() as *mut c_char,
            buffer_size,
            &mut result_type,
        )
    };
    check_api("T32_ExecuteFunction", e)?;
    Ok(Evaluation {
        value: decode_buffer(&buffer)?,
        result_type,
    })
}

pub fn t32_state() -> Result<i32, String> {
    let _guard = connected_api()?;
    let mut state = 0;
    check_api("T32_GetState", unsafe { t32::T32_GetState(&mut state) })?;
    Ok(state)
}

pub fn t32_practice_state() -> Result<i32, String> {
    let _guard = connected_api()?;
    let mut state = 0;
    check_api("T32_GetPracticeState", unsafe {
        t32::T32_GetPracticeState(&mut state)
    })?;
    Ok(state)
}

pub fn t32_read_register(name: &str) -> Result<u64, String> {
    let name = api_string(name, "Register name")?;
    let _guard = connected_api()?;
    let (mut lower, mut upper) = (0, 0);
    check_api("T32_ReadRegisterByName", unsafe {
        t32::T32_ReadRegisterByName(name.as_ptr(), &mut lower, &mut upper)
    })?;
    Ok((u64::from(upper) << 32) | u64::from(lower))
}

fn memory_access_string(access: i32) -> Result<&'static CStr, String> {
    match access {
        0 => Ok(c"D"),
        1 => Ok(c"P"),
        14 => Ok(c"DAP"),
        _ => Err("Memory access must be data (0), program (1), or arm_dap (14)".into()),
    }
}

fn validate_memory_read_state(state: i32, access: i32, dual_port: bool) -> Result<(), String> {
    match state {
        1 | 2 => Ok(()),
        3 if access == 14 && dual_port => Ok(()),
        3 => Err(
            "Running-target memory reads require access=arm_dap and dual_port=true (EDAP). \
             Plain DAP and all data/program reads are refused because generic E-access \
             settings may permit StopAndGo; this tool does not issue explicit run-control commands."
                .into(),
        ),
        _ => Err(format!(
            "Memory reads require an active target; TRACE32 reported state {state}"
        )),
    }
}

struct MemoryObjects {
    address: t32::T32_AddressHandle,
    buffer: t32::T32_BufferHandle,
    release_address: unsafe extern "C" fn(*mut t32::T32_AddressHandle) -> i32,
    release_buffer: unsafe extern "C" fn(*mut t32::T32_BufferHandle) -> i32,
}

impl Default for MemoryObjects {
    fn default() -> Self {
        Self {
            address: ptr::null_mut(),
            buffer: ptr::null_mut(),
            release_address: t32::T32_ReleaseAddressObj,
            release_buffer: t32::T32_ReleaseBufferObj,
        }
    }
}

impl Drop for MemoryObjects {
    fn drop(&mut self) {
        // Objects must be released before the caller's native API lock is dropped.
        unsafe {
            if !self.buffer.is_null() {
                (self.release_buffer)(&mut self.buffer);
            }
            if !self.address.is_null() {
                (self.release_address)(&mut self.address);
            }
        }
    }
}

fn validate_memory_buffer(
    used: u32,
    capacity: u32,
    storage_is_null: bool,
    length: usize,
) -> Result<(), String> {
    if used as usize != length || capacity < used || storage_is_null {
        return Err(format!(
            "T32_ReadMemoryObj returned an invalid buffer: expected {length} bytes, \
             received {used}, capacity {capacity}"
        ));
    }
    Ok(())
}

pub fn t32_read_memory(
    address: u32,
    length: usize,
    access: i32,
    dual_port: bool,
) -> Result<Vec<u8>, String> {
    if length == 0 || length > 4096 || u64::from(address) + length as u64 > 1u64 << 32 {
        return Err("Memory reads require 1..4096 bytes within the 32-bit address space".into());
    }
    let access_string = memory_access_string(access)?;
    let _guard = connected_api()?;
    let mut objects = MemoryObjects::default();
    check_api("T32_RequestAddressObjA32", unsafe {
        t32::T32_RequestAddressObjA32(&mut objects.address, address)
    })?;
    if objects.address.is_null() {
        return Err("T32_RequestAddressObjA32 returned a null address object".into());
    }
    check_api("T32_SetAddressObjAccessString", unsafe {
        t32::T32_SetAddressObjAccessString(objects.address, access_string.as_ptr())
    })?;
    if dual_port {
        check_api("T32_SetAddressObjAttr", unsafe {
            t32::T32_SetAddressObjAttr(objects.address, t32::T32_ADDROBJATTR_EACCESS)
        })?;
    }
    check_api("T32_RequestBufferObj", unsafe {
        t32::T32_RequestBufferObj(&mut objects.buffer, length as i32)
    })?;
    if objects.buffer.is_null() {
        return Err("T32_RequestBufferObj returned a null buffer object".into());
    }

    // Check immediately before reading; do not call t32_state() while holding API_LOCK.
    let mut state = 0;
    check_api("T32_GetState", unsafe { t32::T32_GetState(&mut state) })?;
    validate_memory_read_state(state, access, dual_port)?;
    check_api("T32_ReadMemoryObj", unsafe {
        t32::T32_ReadMemoryObj(objects.buffer, objects.address, length as u32)
    })?;
    let native_buffer = unsafe { &*objects.buffer };
    validate_memory_buffer(
        native_buffer.used,
        native_buffer.bufsize,
        native_buffer.storage.is_null(),
        length,
    )?;
    let mut buffer = vec![0; length];
    check_api("T32_CopyDataFromBufferObj", unsafe {
        t32::T32_CopyDataFromBufferObj(buffer.as_mut_ptr(), length as i32, objects.buffer)
    })?;
    Ok(buffer)
}

#[derive(Debug, serde::Serialize)]
pub struct SymbolInfo {
    pub address: u32,
    pub size: u32,
    /// Raw legacy API field; the current vendor manual describes it as reserved.
    pub access: u32,
}

fn finish_symbol_lookup(name: &str, info: SymbolInfo, status: i32) -> Result<SymbolInfo, String> {
    check_api("T32_GetSymbol", status)?;
    // The documented missing-symbol sentinel excludes the reserved third field.
    if info.address == u32::MAX && info.size == u32::MAX {
        return Err(format!(
            "Symbol {name:?} was not found in loaded debug symbols"
        ));
    }
    Ok(info)
}

pub fn t32_lookup_symbol(name: &str) -> Result<SymbolInfo, String> {
    let c_name = api_string(name, "Symbol name")?;
    let _guard = connected_api()?;
    let mut info = SymbolInfo {
        address: 0,
        size: 0,
        access: 0,
    };
    let status = unsafe {
        t32::T32_GetSymbol(
            c_name.as_ptr(),
            &mut info.address,
            &mut info.size,
            &mut info.access,
        )
    };
    finish_symbol_lookup(name, info, status)
}

#[derive(Debug, serde::Serialize)]
pub struct WindowContent {
    pub text: String,
    pub bytes_read: usize,
}

pub fn t32_read_window(command: &str, offset: u32, length: usize) -> Result<WindowContent, String> {
    if length == 0 || length > 8192 || offset.checked_add(length as u32).is_none() {
        return Err("Window reads require 1..8192 bytes and a non-overflowing offset".into());
    }
    let command = api_string(command, "Window command")?;
    let _guard = connected_api()?;
    let mut buffer = vec![0; length + 1];
    let count = unsafe {
        t32::T32_GetWindowContent(
            command.as_ptr(),
            buffer.as_mut_ptr() as *mut c_char,
            buffer.len() as u32,
            offset,
            t32::T32_PRINTCODE_ASCII,
        )
    };
    if count < 0 {
        return Err("T32_GetWindowContent failed; this window may not be supported by the active PowerView configuration".into());
    }
    if count as usize > length {
        return Err("T32_GetWindowContent returned an invalid byte count".into());
    }
    Ok(WindowContent {
        text: String::from_utf8_lossy(&buffer[..count as usize]).into_owned(),
        bytes_read: count as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_is_an_error_not_a_panic() {
        assert!(t32_cmd("PRINT\0x".into()).is_err());
        assert!(t32_eval("x\0").is_err());
        assert!(t32_read_register("PC\0").is_err());
        assert!(t32_lookup_symbol("main\0").is_err());
    }

    #[test]
    fn disconnected_native_queries_are_errors_not_null_pointer_calls() {
        t32_disconnect();
        assert!(t32_state().is_err());
        assert!(t32_practice_state().is_err());
        assert!(t32_cmd("PRINT 1".into()).is_err());
        assert!(t32_eval("1").is_err());
        assert!(t32_read_register("PC").is_err());
        assert!(t32_lookup_symbol("main").is_err());
        assert!(t32_read_memory(0, 1, 0, false).is_err());
        assert!(t32_read_window("Register", 0, 1).is_err());
        t32_disconnect();
    }

    #[test]
    fn bounded_decoding_requires_terminator() {
        assert_eq!(decode_buffer(b"abc\0tail").unwrap(), "abc");
        assert!(decode_buffer(b"no terminator").is_err());
    }

    #[test]
    fn invalid_memory_or_window_bounds_never_call_ffi() {
        assert!(t32_read_memory(0, 0, 0, false).is_err());
        assert!(t32_read_memory(0, 4097, 0, false).is_err());
        assert!(t32_read_memory(u32::MAX, 2, 0, false).is_err());
        assert!(
            t32_read_memory(0, 1, 2, false)
                .unwrap_err()
                .contains("Memory access must be")
        );
        assert!(t32_read_window("Register", 0, 0).is_err());
        assert!(t32_read_window("Register", u32::MAX, 2).is_err());
        assert!(t32_connect(0).is_err());
    }

    #[test]
    fn memory_access_strings_preserve_supported_numeric_mapping() {
        assert_eq!(memory_access_string(0).unwrap(), c"D");
        assert_eq!(memory_access_string(1).unwrap(), c"P");
        assert_eq!(memory_access_string(14).unwrap(), c"DAP");
        for access in [i32::MIN, -1, 2, 13, 15, i32::MAX] {
            assert!(memory_access_string(access).is_err());
        }
    }

    #[test]
    fn symbol_lookup_rejects_missing_address_and_size_regardless_of_reserved_field() {
        for access in [0, 0x1000_0000, u32::MAX] {
            let missing = SymbolInfo {
                address: u32::MAX,
                size: u32::MAX,
                access,
            };
            let error = finish_symbol_lookup("missing_symbol", missing, 0).unwrap_err();
            assert!(error.contains("missing_symbol"));
            assert!(error.contains("not found"));
        }

        for (address, size, access) in [
            (0x0800_E930, 188, 0x1000_0000),
            (u32::MAX, 1, 0),
            (u32::MAX, 0, u32::MAX),
            (0, u32::MAX, u32::MAX),
            (0, 0, 0),
        ] {
            let info = SymbolInfo {
                address,
                size,
                access,
            };
            let result = finish_symbol_lookup("valid_symbol", info, 0).unwrap();
            assert_eq!(
                (result.address, result.size, result.access),
                (address, size, access)
            );
        }
    }

    #[test]
    fn symbol_lookup_preserves_native_errors_before_interpreting_metadata() {
        let missing = SymbolInfo {
            address: u32::MAX,
            size: u32::MAX,
            access: u32::MAX,
        };
        assert_eq!(
            finish_symbol_lookup("symbol", missing, 4113).unwrap_err(),
            "T32_GetSymbol failed (Remote API error 4113)"
        );
    }

    #[test]
    fn running_memory_reads_require_explicit_edap() {
        for access in [0, 1, 14] {
            for dual_port in [false, true] {
                for state in [1, 2] {
                    assert!(validate_memory_read_state(state, access, dual_port).is_ok());
                }
                assert_eq!(
                    validate_memory_read_state(3, access, dual_port).is_ok(),
                    access == 14 && dual_port
                );
                for state in [i32::MIN, -1, 0, 4, i32::MAX] {
                    assert!(validate_memory_read_state(state, access, dual_port).is_err());
                }
            }
        }
    }

    #[test]
    fn memory_buffer_requires_exact_length_and_valid_storage() {
        assert!(validate_memory_buffer(16, 16, false, 16).is_ok());
        assert!(validate_memory_buffer(16, 32, false, 16).is_ok());
        for (used, capacity, storage_is_null) in [
            (0, 16, false),
            (15, 16, false),
            (17, 32, false),
            (16, 15, false),
            (16, 16, true),
        ] {
            assert!(validate_memory_buffer(used, capacity, storage_is_null, 16).is_err());
        }
    }

    #[test]
    fn memory_objects_release_partial_and_complete_ownership_on_every_exit() {
        thread_local! {
            static RELEASED: Cell<u32> = const { Cell::new(0) };
        }
        unsafe extern "C" fn release_address(handle: *mut t32::T32_AddressHandle) -> i32 {
            assert!(!unsafe { *handle }.is_null());
            RELEASED.set(RELEASED.get() * 10 + 1);
            unsafe { *handle = ptr::null_mut() };
            0
        }
        unsafe extern "C" fn release_buffer(handle: *mut t32::T32_BufferHandle) -> i32 {
            assert!(!unsafe { *handle }.is_null());
            RELEASED.set(RELEASED.get() * 10 + 2);
            unsafe { *handle = ptr::null_mut() };
            0
        }
        for (address, buffer, expected) in [
            (ptr::null_mut(), ptr::null_mut(), 0),
            (ptr::dangling_mut(), ptr::null_mut(), 1),
            (ptr::null_mut(), ptr::dangling_mut(), 2),
            (ptr::dangling_mut(), ptr::dangling_mut(), 21),
        ] {
            for return_error in [false, true] {
                RELEASED.set(0);
                let result = (|| -> Result<(), String> {
                    let _objects = MemoryObjects {
                        address,
                        buffer,
                        release_address,
                        release_buffer,
                    };
                    if return_error {
                        return Err("injected failure".into());
                    }
                    Ok(())
                })();
                assert_eq!(result.is_err(), return_error);
                assert_eq!(RELEASED.get(), expected);
            }
        }
    }
}
