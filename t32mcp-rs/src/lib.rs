// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Communication with TRACE32. Built on top of the Remote API.
//!
//! Pipe communication:
//! - Windows: Named Pipe
//! - Linux: FIFO

use anyhow::Result;
use std::sync::{Mutex, OnceLock};

pub mod t32api;

use t32api::{Connection, T32_DEV_ICD};

/// The process-wide TRACE32 Remote API connection
/// (equivalent to the implicit channel of the C Remote API library).
fn channel() -> &'static Mutex<Option<Connection>> {
    static CHANNEL: OnceLock<Mutex<Option<Connection>>> = OnceLock::new();
    CHANNEL.get_or_init(|| Mutex::new(None))
}

fn with_channel<T>(f: impl FnOnce(&mut Connection) -> Result<T, String>) -> Result<T, String> {
    let mut guard = channel()
        .lock()
        .map_err(|e| format!("TRACE32 connection lock poisoned: {e}"))?;
    let conn = guard
        .as_mut()
        .ok_or_else(|| "not connected to TRACE32".to_string())?;
    f(conn)
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
        .expect("cannot create pipe");

    let area = t32_fnc(r"AREA.SELECTed()".to_string())?;
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
    let mut connection =
        Connection::connect("localhost", port, Some(std::time::Duration::from_secs(60)))
            .map_err(|e| format!("Connection with TRACE32 failed: {e}"))?;

    connection
        .attach(T32_DEV_ICD as u8)
        .map_err(|e| format!("Connection with TRACE32 failed: T32_Attach: {e}"))?;

    {
        let mut guard = channel()
            .lock()
            .map_err(|e| format!("TRACE32 connection lock poisoned: {e}"))?;
        *guard = Some(connection);
    }

    let area = t32_fnc(r"AREA.SELECTed()".to_string())?;
    t32_cmd(format!(r"AREA.OPEN {area}"))?;

    Ok(())
}

pub fn t32_cmd(cmd: String) -> Result<(), String> {
    with_channel(|c| c.execute_command(&cmd)).map_err(|e| format!("TRACE32 command failed: {e}"))
}

pub fn t32_fnc(fnc: String) -> Result<String, String> {
    let (value, _result_type) = with_channel(|c| c.execute_function(&fnc))
        .map_err(|e| format!("TRACE32 function failed: {e}"))?;
    Ok(value)
}
