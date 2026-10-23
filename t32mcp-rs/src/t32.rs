// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::request_budget::{self, RequestBudget};
use crate::t32api::{Connection, T32_DEV_ICD};

#[cfg(target_os = "windows")]
use interprocess::os::windows::named_pipe::PipeListenerOptions;
#[cfg(target_os = "windows")]
use interprocess::os::windows::named_pipe::pipe_mode;

#[cfg(not(target_os = "windows"))]
use interprocess::os::unix::fifo_file;
#[cfg(not(target_os = "windows"))]
use std::os::unix::fs::OpenOptionsExt;

/// Read end of the TRACE32 AREA output pipe:
/// - Windows: named pipe
/// - Unix: FIFO opened in non-blocking mode
#[cfg(target_os = "windows")]
type PipeStream =
    interprocess::os::windows::named_pipe::PipeStream<pipe_mode::Bytes, pipe_mode::None>;
#[cfg(not(target_os = "windows"))]
type PipeStream = std::fs::File;

/// Whether a read of 0 bytes means that PowerView closed the pipe.
///
/// A non-blocking FIFO read returns 0 bytes only after PowerView closed the
/// pipe.
#[cfg(not(target_os = "windows"))]
fn pipe_closed(_stream: &PipeStream) -> bool {
    true
}

/// Whether a read of 0 bytes means that PowerView closed the pipe.
///
/// A non-blocking named pipe read returns 0 bytes both if no data is
/// available and after PowerView closed the pipe: interprocess maps the
/// errors of both cases to EOF. PeekNamedPipe tells them apart, it fails
/// (ERROR_BROKEN_PIPE) only for a closed pipe.
#[cfg(target_os = "windows")]
fn pipe_closed(stream: &PipeStream) -> bool {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn PeekNamedPipe(
            pipe: *mut c_void,
            buffer: *mut c_void,
            buffer_size: u32,
            bytes_read: *mut u32,
            total_bytes_avail: *mut u32,
            bytes_left_this_message: *mut u32,
        ) -> i32;
    }

    let mut available = 0u32;
    // SAFETY: the handle is valid while `stream` is borrowed, all other
    // pointers are either null (optional) or point to a local.
    let ok = unsafe {
        PeekNamedPipe(
            stream.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    ok == 0
}

/// POSIX `O_NONBLOCK` (0o4000 on Linux, not exposed by std).
#[cfg(not(target_os = "windows"))]
const O_NONBLOCK: i32 = 0o4000;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// AREA that is piped and receives the output of the scripts run by t32mcp.
/// Error, warning and info messages always go to A000, whatever AREA is
/// selected, so only A000 carries the complete output.
const MCP_AREA: &str = "A000";

/// Printed once per new pipe to verify it. The user sees it in the AREA
/// window.
const PIPE_CHECK_MESSAGE: &str = "t32mcp: AREA A000 is piped to the MCP server";

/// Printed on a line of its own by the error handler of a script that caught
/// an error itself (`ON ERROR JUMPTO`): the call returns a failed tool result
/// with the full output.
const SCRIPT_FAILED_MARKER: &str = "[t32mcp: script failed]";

/// Target state reported by `STATE.TARGET()` when the core is not usable.
const STATE_SYSTEM_DOWN: &str = "system down";

struct PowerView {
    port: u16,
    connection: Option<Connection>,
    stream: Option<PipeStream>,
    /// The AREA that `stream` receives, see `AREA.PIPE`.
    piped_area: Option<String>,
}

impl PowerView {
    /// An instance that is connected lazily on its first request.
    fn new(port: u16) -> Self {
        Self {
            port,
            connection: None,
            stream: None,
            piped_area: None,
        }
    }

    fn ensure_rcl_connected(&mut self, deadline: Instant) -> Result<(), String> {
        // At most one reconnect, sharing the caller's budget. A peer that
        // immediately fails health checks must not cause recursive retries.
        for attempt in 0..2 {
            let remaining = request_budget::remaining(deadline)?;
            if self.connection.is_none() {
                let port = self.port;
                eprintln!("Connecting RCL: 127.0.0.1:{port}");
                let mut connection =
                    Connection::connect("127.0.0.1", port, Some(remaining.min(DEFAULT_TIMEOUT)))?;
                connection.set_deadline(Some(deadline));
                connection.set_timeout(Some(HEALTH_CHECK_TIMEOUT))?;
                connection
                    .attach(T32_DEV_ICD as u8)
                    .map_err(|e| format!("T32_Attach failed: {e}. PowerView does not respond, e.g. it is busy, hangs, or a modal dialog (DIALOG.OK, DIALOG.YESNO, DIALOG.File) blocks it (Linux). Ask the user to check PowerView and to close open dialogs."))?;
                self.connection = Some(connection);
            }

            let connection = self.connection.as_mut().unwrap();
            connection.set_deadline(Some(deadline));
            let health = connection
                .set_timeout(Some(HEALTH_CHECK_TIMEOUT))
                .and_then(|()| connection.execute_function("PRACTICE.StackDepth()"));
            if health.is_ok() {
                connection.set_timeout(Some(DEFAULT_TIMEOUT))?;
                eprintln!("RCL connection valid: {}", self.port);
                return Ok(());
            }

            // Keep the pipe while reconnecting: PowerView may still write
            // to it even when its Remote API is temporarily blocked.
            self.connection = None;
            if attempt == 1 {
                return Err(format!("RCL health check failed: {}", health.unwrap_err()));
            }
        }
        unreachable!("the bounded reconnect loop returns on its final attempt")
    }

    fn ensure_connected(&mut self, deadline: Instant) -> Result<(), String> {
        // One retry: a stale pipe is replaced by a fresh one.
        match self.try_ensure_connected(deadline) {
            Ok(()) => Ok(()),
            Err(_) => {
                request_budget::remaining(deadline)?;
                self.try_ensure_connected(deadline)
            }
        }
    }

    fn try_ensure_connected(&mut self, deadline: Instant) -> Result<(), String> {
        self.ensure_rcl_connected(deadline)?;

        // PowerView closed the pipe, e.g. after a restart or AREA.CLOSE:
        // pipe a new one.
        if let Some(stream) = self.stream.as_mut()
            && pipe_ended(stream, deadline)
        {
            eprintln!("AREA output pipe was closed, piping a new one");
            self.stream = None;
            self.piped_area = None;
        }
        // Only a new pipe is verified. The ECHO for it shows up in the user's
        // AREA window.
        let new_pipe = self.stream.is_none();

        #[cfg(target_os = "windows")]
        if self.stream.is_none() {
            let port = self.port;
            let pipe_name = format!("t32mcp.{}.{port}", std::process::id());
            let pipe_path = format!(r"\\.\pipe\{pipe_name}");

            let connection = self
                .connection
                .as_mut()
                .expect("TRACE32 connection not established");

            let listener = PipeListenerOptions::new()
                .nonblocking(true)
                .path(&*pipe_path)
                .create_recv_only::<pipe_mode::Bytes>()
                .map_err(|e| format!("Cannot create named pipe listener: {e}"))?;

            connection.execute_command(&format!(r"AREA.PIPE {MCP_AREA} {pipe_path}"))?;

            let stream = listener
                .accept()
                .map_err(|e| format!("Cannot accept pipe connection: {e}"))?;

            self.stream = Some(stream);
            self.piped_area = Some(MCP_AREA.to_string());
        }

        #[cfg(not(target_os = "windows"))]
        if self.stream.is_none() {
            let port = self.port;
            // Every attach needs its own FIFO path: TRACE32 deletes the path
            // of a pipe when it detaches it, so re-using a path that a stale
            // pipe still refers to would destroy the freshly created FIFO.
            static FIFO_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let id = FIFO_ID.fetch_add(1, Ordering::Relaxed);
            let fifo_name = format!("/tmp/t32mcp.{}.{port}.{id}", std::process::id());
            // Remove a stale FIFO left over from a previous run before re-creating it.
            let _ = std::fs::remove_file(&fifo_name);
            if let Err(e) = fifo_file::create_fifo(&fifo_name, 0o777) {
                return Err(format!("FIFO cannot be created: {e}"));
            }

            // Open the read end before TRACE32 attaches to the FIFO so its
            // writer side cannot block. O_NONBLOCK makes the read end poll:
            // reads return WouldBlock instead of blocking.
            let stream = match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(O_NONBLOCK)
                .open(&fifo_name)
            {
                Ok(stream) => stream,
                Err(e) => {
                    let _ = std::fs::remove_file(&fifo_name);
                    return Err(format!("Cannot connect to FIFO: {e}"));
                }
            };

            let connection = self
                .connection
                .as_mut()
                .expect("TRACE32 connection not established");

            let attached = connection.execute_command(&format!("AREA.PIPE {MCP_AREA} {fifo_name}"));

            // The receiver holds an open fd; unlink the path so it is cleaned up on close.
            let _ = std::fs::remove_file(&fifo_name);
            attached?;

            self.stream = Some(stream);
            self.piped_area = Some(MCP_AREA.to_string());
        }

        if !new_pipe {
            return Ok(());
        }

        let connection = self
            .connection
            .as_mut()
            .expect("TRACE32 connection not established");

        let original_area = select_mcp_area(connection)?;
        let echoed =
            connection.execute_command(&format!("PRINT %WARNING \"{PIPE_CHECK_MESSAGE}\""));
        restore_area(connection, &original_area);
        echoed?;

        let verify_stream = self.stream.as_mut().expect("pipe stream not available");
        let mut buf = vec![0u8; 64 * 1024];
        let verify_deadline = deadline.min(Instant::now() + HEALTH_CHECK_TIMEOUT);
        let mut received = String::new();
        loop {
            if Instant::now() >= verify_deadline {
                break;
            }
            match verify_stream.read(&mut buf) {
                Ok(0) if pipe_closed(verify_stream) => break,
                Ok(0) => {}
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    received.push_str(&text);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            if received.contains(PIPE_CHECK_MESSAGE) || Instant::now() >= verify_deadline {
                break;
            }
            thread::sleep(
                verify_deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100)),
            );
        }

        if received.contains(PIPE_CHECK_MESSAGE) {
            eprintln!("pipe connection established");
            return Ok(());
        }

        self.detach_pipe_before(deadline);
        Err("AREA output pipe could not be verified".to_string())
    }

    /// Detach the AREA output pipe. Otherwise PowerView keeps writing to a
    /// FIFO without reader after t32mcp exits, and the next PRINT in a
    /// user's script fails with "critical error accessing file pipe".
    fn detach_pipe(&mut self) {
        self.detach_pipe_before(Instant::now() + HEALTH_CHECK_TIMEOUT);
    }

    fn detach_pipe_before(&mut self, deadline: Instant) {
        if let (Some(connection), Some(area)) = (self.connection.as_mut(), self.piped_area.take()) {
            connection.set_deadline(Some(deadline));
            let _ = connection.set_timeout(Some(HEALTH_CHECK_TIMEOUT));
            if area_exists(connection, &area).unwrap_or(false) {
                let _ = connection.execute_command(&format!("AREA.CLOSE {area}"));
            }
        }
        self.stream = None;
    }

    /// Query all cores of this instance. Each core reports its TRACE32 core
    /// number (`core`, see `CORE()`) and whether it is the selected one.
    fn query_instance(
        &mut self,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Vec<serde_json::Value>, String> {
        let connection = self
            .connection
            .as_mut()
            .expect("TRACE32 connection not established");

        let original_core = connection.fnc_u32(deadline, timeout, "CORE()")?;

        // A core count of 0 is unexpected on single-core setups; fall back
        // to the currently selected core.
        let core_count = connection
            .fnc_u32(deadline, timeout, "CORE.NUMBER()")
            .unwrap_or(0);
        let core_ids: Vec<u32> = if core_count == 0 {
            vec![original_core]
        } else {
            (0..core_count).collect()
        };

        let mut cores = Vec::new();
        for id in core_ids {
            request_budget::remaining(deadline)?;
            // Per-core query: any error is reported as the core's "error"
            // entry instead of failing the whole instance.
            let core = (|| -> Result<Value, String> {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(format!(
                        "query timed out after {} seconds",
                        timeout.as_secs()
                    ));
                }
                connection.set_timeout(Some(remaining))?;

                if core_count > 1 {
                    // CORE.Select is unavailable for single-core
                    connection
                        .execute_command(&format!("CORE.Select {id}."))
                        .map_err(|e| format!("cannot select core {id}: {e}"))?;
                }

                let cpu_family = connection.fnc_string(deadline, timeout, "CPUFAMILY()")?;
                let cpu = connection.fnc_string(deadline, timeout, "CPU()")?;
                let running = connection.fnc_bool(deadline, timeout, "STATE.RUN()")?;
                let state = connection.fnc_string(deadline, timeout, "STATE.TARGET()")?;

                let mut core = json!({
                    "core": id,
                    "selected": id == original_core,
                    "cpu_family": cpu_family,
                    "cpu": cpu,
                    "state": state,
                    "running": running,
                });

                // Reading the registers of a running core fails (STD_RUNNING):
                // only halted cores are queried. Registers without any value
                // are omitted.
                if !running && state != STATE_SYSTEM_DOWN {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(format!(
                            "query timed out after {} seconds",
                            timeout.as_secs()
                        ));
                    }
                    connection.set_timeout(Some(remaining))?;
                    match connection.read_registers_all(id as u16) {
                        Ok(registers) => {
                            let mut registers_json = Vec::new();
                            for reg in &registers {
                                let Some(name) = &reg.name else {
                                    continue;
                                };
                                let value = if let Some(value) = reg.value {
                                    json!(format!("0x{value:X}"))
                                } else if !reg.values.is_empty() {
                                    Value::Array(
                                        reg.values
                                            .iter()
                                            .map(|v| json!(format!("0x{v:X}")))
                                            .collect(),
                                    )
                                } else {
                                    continue;
                                };
                                registers_json.push(json!({
                                    "name": name,
                                    "value": value,
                                }));
                            }
                            core["registers"] = Value::Array(registers_json);
                        }
                        Err(e) => {
                            core["error"] = Value::String(format!("cannot read registers: {e}"));
                        }
                    }
                }

                Ok(core)
            })()
            .unwrap_or_else(|e| json!({ "core": id, "error": e }));
            cores.push(core);
        }

        restore_core(connection, original_core);

        Ok(cores)
    }

    /// Submit a single PRACTICE command via the Remote API without waiting
    /// for its execution (fire-and-forget). `core` selects the core first;
    /// None keeps the current selection.
    fn run_command(
        &mut self,
        command: &str,
        core: Option<u32>,
        device_prompt: &str,
    ) -> Result<(), String> {
        let connection = self
            .connection
            .as_mut()
            .expect("TRACE32 connection not established");

        if let Some(core) = core {
            select_core(connection, core)?;
        }

        connection.execute_command(device_prompt)?;
        connection.execute_command(command)?;

        Ok(())
    }

    /// Execute a PRACTICE script on this instance. `core` selects the core
    /// before the script runs; None keeps the current selection.
    /// `device_prompt` is selected before the script runs.
    #[allow(clippy::too_many_arguments)]
    fn execute_script(
        &mut self,
        script_path: String,
        args: String,
        core: Option<u32>,
        device_prompt: &str,
        deadline: Instant,
        timeout: Duration,
        if_busy: IfBusy,
    ) -> Result<String, String> {
        let connection = self
            .connection
            .as_mut()
            .expect("TRACE32 connection not established");

        // A script on the PRACTICE stack may be important, e.g. still running
        // after a timeout or paused in a dialog. The agent decides.
        let busy = practice_state(connection)?;
        match (busy["state"].as_str(), if_busy) {
            (Some("idle"), _) => {}
            (_, IfBusy::Fail) => {
                return Err(format!(
                    "PowerView is busy. {}\nRetry with if_busy=\"terminate\" to end the scripts first, or with if_busy=\"nest\" to run on top of a paused script, e.g. to operate its dialog.",
                    describe_practice(&busy)
                ));
            }
            (Some("running"), IfBusy::Nest) => {
                return Err(format!(
                    "Cannot nest on a running script. {}\nWait and check with trace32_get_status, or retry with if_busy=\"terminate\".",
                    describe_practice(&busy)
                ));
            }
            (_, IfBusy::Terminate) => {
                clear_practice_stack(connection)?;
            }
            (_, IfBusy::Nest) => {}
        }

        connection.set_timeout(Some(request_budget::remaining(deadline)?))?;

        let original_core = connection.fnc_u32(deadline, timeout, "CORE()")?;

        // Select the requested core before the script runs. The selection is
        // not restored afterwards: the script may deliberately change it.
        if let Some(core) = core {
            select_core(connection, core)?;
        }

        // ERROR.OCCURRED() then tells an error from a STOP if the script halts.
        connection.execute_command("ERROR.RESet")?;
        let original_area = select_mcp_area(connection)?;
        // The piped AREA also receives messages between the calls, e.g. of
        // the user's work or of a timed-out script. They do not belong to
        // this call.
        if let Some(stream) = self.stream.as_mut() {
            drain_pipe(stream, deadline);
        }
        // A script runs with the device prompt selected in the PowerView command
        // line, e.g. "::" typed by the user: debugger commands then fail with
        // "unknown command". A prompt before DO only applies to the DO line,
        // a prompt alone selects it permanently.
        if let Err(e) = connection
            .execute_command(device_prompt)
            .and_then(|()| connection.execute_command(&format!("DO \"{}\"{}", script_path, args)))
        {
            restore_area(connection, &original_area);
            return Err(e);
        }

        // Wait for script termination: poll the PRACTICE stack depth, then
        // send an end marker through the piped AREA and collect the pipe
        // output up to that marker.
        let end_marker = format!("[t32mcp (PID={}): output-end-marker]", std::process::id());

        let mut stream = self.stream.take().expect("pipe stream not available");

        let stop = Arc::new(AtomicBool::new(false));

        let marker_for_pipe = end_marker.clone();
        let stop_pipe = Arc::clone(&stop);
        // Returns the output and whether PowerView closed the pipe, e.g. by
        // AREA.CLOSE or AREA.RESet in the script.
        let pipe_handle = thread::spawn(move || -> (String, PipeStream, bool) {
            let mut output = String::new();
            let mut buf = vec![0u8; 64 * 1024];
            let mut closed = false;
            loop {
                match stream.read(&mut buf) {
                    Ok(0) if pipe_closed(&stream) => closed = true,
                    Ok(0) => {}
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        output.push_str(&text);
                    }
                    // Busy polling on purpose: PowerView buffers output for a
                    // full pipe only up to a limit, then the script fails with
                    // "no memory for file pipe" (e.g. large WinPrint output).
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        eprintln!("pipe read failed: {e}");
                        closed = true;
                    }
                }

                if closed
                    || output.trim_end().ends_with(&marker_for_pipe)
                    || stop_pipe.load(Ordering::Relaxed)
                    || Instant::now() >= deadline
                {
                    break;
                }
            }

            (output, stream, closed)
        });

        let mut marker_sent = false;
        let mut halted_polls = 0;
        let mut halt_reason = None;
        let mut paused = None;
        let mut switched_area = None;
        let mut t32_err: Result<(), String> = Ok(());
        loop {
            if Instant::now() >= deadline {
                stop.store(true, Ordering::SeqCst);
                break;
            }
            if !marker_sent {
                match connection.execute_function("PRACTICE.StackDepth()") {
                    Ok((depth, _)) => {
                        // A script that halted at an error or at STOP stays
                        // on the stack, but PRACTICE no longer runs. Require
                        // two polls in a row so a script that is just starting
                        // is not mistaken for a halted one. A script halted at
                        // an error is terminated, a paused one keeps waiting.
                        let mut idle = depth == "0.";
                        if !idle {
                            match connection.practice_running() {
                                Ok(false) => halted_polls += 1,
                                Ok(true) => halted_polls = 0,
                                Err(e) => {
                                    t32_err = Err(e);
                                    stop.store(true, Ordering::SeqCst);
                                    break;
                                }
                            }
                            if halted_polls >= 2 {
                                if let Some(reason) = error_of(connection) {
                                    halt_reason = Some(reason);
                                    if let Err(e) = clear_practice_stack(connection)
                                        .and_then(|()| connection.execute_command("ERROR.RESet"))
                                    {
                                        t32_err = Err(e);
                                        stop.store(true, Ordering::SeqCst);
                                        break;
                                    }
                                    continue;
                                }
                                paused = practice_state(connection).ok();
                                idle = true;
                            }
                        }
                        if idle {
                            // The script or the user may have selected another
                            // AREA, e.g. with AREA.Create. Output after that
                            // went there.
                            if let Ok((area, _)) = connection.execute_function("AREA.SELECTed()")
                                && area != MCP_AREA
                                && !area.is_empty()
                            {
                                switched_area = Some(area);
                            }
                            if let Err(e) = connection
                                .execute_command(&format!("AREA.Select {MCP_AREA}"))
                                .and_then(|()| {
                                    connection.execute_command(&format!(
                                        "PRINT %WARNING \"{end_marker}\""
                                    ))
                                })
                            {
                                t32_err = Err(e);
                                stop.store(true, Ordering::SeqCst);
                                break;
                            }
                            marker_sent = true;
                        }
                    }
                    Err(e) => {
                        t32_err = Err(e);
                        stop.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }

            if pipe_handle.is_finished() {
                break;
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100)),
            );
        }

        let (output, stream, pipe_closed) = pipe_handle.join().expect("pipe thread panicked");
        if pipe_closed {
            // Pipe a new one on the next call.
            self.piped_area = None;
        } else {
            self.stream = Some(stream);
        }

        // Expiry stops local waiting, not the remote script. Do not issue
        // cleanup or state queries with a fresh timeout after this point.
        if Instant::now() >= deadline {
            return Err(format!(
                "{}\n\nOutput so far:\n{output}",
                request_budget::unknown_completion()
            ));
        }

        // Do not restore the core of a blocked PowerView: every further call
        // would wait for the socket timeout.
        if let Err(e) = t32_err {
            let _ = connection.set_timeout(Some(DEFAULT_TIMEOUT));
            return Err(format!(
                "{e}. PowerView does not respond, e.g. it is busy, hangs, or a modal dialog (DIALOG.OK, DIALOG.YESNO, DIALOG.File) blocks it (Linux). Ask the user to check PowerView and to close open dialogs."
            ));
        }
        restore_core(connection, original_core);
        let timed_out = !pipe_closed && !output.trim_end().ends_with(&end_marker);
        // A script that still runs keeps printing: leave the piped AREA
        // selected. Keep an AREA that was selected deliberately during the
        // script.
        if !timed_out {
            restore_area(
                connection,
                switched_area.as_deref().unwrap_or(&original_area),
            );
        }
        let area_warning = switched_area
            .as_ref()
            .map(|area| format!("\nWarning: the selected AREA changed to {area} while the script ran, e.g. by AREA.Select or AREA.Create in the script or in PowerView. Output printed after that is missing here. Read it with PRINT AREA.LINE(\"{area}\",<line>) (0 = last line, -1 = the one before)."));
        let _ = connection.set_timeout(Some(DEFAULT_TIMEOUT));

        if output.trim_end().ends_with(&end_marker) {
            let stripped = output
                .trim_end()
                .strip_suffix(&end_marker)
                .unwrap_or(&output);
            let stripped = &format!("{stripped}{}", area_warning.unwrap_or_default());
            match halt_reason {
                None => match paused {
                    Some(state) => Ok(format!(
                        "{stripped}\n\n{}\nTo operate its dialog, use trace32_run_command with if_busy=\"nest\": DIALOG.SELect <dialog>, then DIALOG.Set <label> <value>, DIALOG.EXecute <label>, PRINT DIALOG.STRing(<label>). CONTinue (with if_busy=\"nest\") resumes the script, if_busy=\"terminate\" ends it.",
                        describe_practice(&state)
                    )),
                    None if stripped
                        .lines()
                        .any(|line| line.trim() == SCRIPT_FAILED_MARKER) =>
                    {
                        Err(stripped.to_string())
                    }
                    None => Ok(stripped.to_string()),
                },
                Some(reason) if stripped.trim().is_empty() => Err(reason),
                Some(reason) => Err(format!("{reason}\n\nOutput before the halt:\n{stripped}")),
            }
        } else if pipe_closed {
            let mut message = "The AREA output pipe was closed, e.g. by AREA.CLOSE or AREA.RESet in the script. The output is incomplete.".to_string();
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(200)),
            );
            if let Ok(state) = practice_state(connection) {
                if state["state"] == "error" {
                    if let Some(reason) = error_of(connection) {
                        message = format!("{reason}\n{message}");
                    }
                    let _ = clear_practice_stack(connection)
                        .and_then(|()| connection.execute_command("ERROR.RESet"));
                } else if state["state"] != "idle" {
                    message.push_str(&format!(" {}", describe_practice(&state)));
                }
            }
            Err(format!("{message}\n\nOutput so far:\n{output}"))
        } else {
            let mut message = format!(
                "Script execution timed out after {} seconds.",
                timeout.as_secs()
            );
            if let Ok(state) = practice_state(connection)
                && state["state"] != "idle"
            {
                message.push_str(&format!(
                    " It keeps running. {}\nCheck it with trace32_get_status, or end it with if_busy=\"terminate\" on the next call. If it waits in a modal dialog (DIALOG.OK, DIALOG.YESNO, DIALOG.File), ask the user to close it: terminating the script leaves the dialog open.",
                    describe_practice(&state)
                ));
                // The next call discards the pipe content of the meantime.
                message.push_str(&format!(
                    "\nIts further output goes to AREA {MCP_AREA}, but not into later results. Read it with PRINT AREA.LINE(\"{MCP_AREA}\",<line>) (0 = last line, -1 = the one before)."
                ));
                if original_area != MCP_AREA && !original_area.is_empty() {
                    message.push_str(&format!(
                        "\nWarning: AREA {original_area} was selected before the call. {MCP_AREA} stays selected because the script still runs."
                    ));
                }
            } else {
                restore_area(connection, &original_area);
            }
            Err(format!(
                "{message}{}\n\nOutput so far:\n{output}",
                area_warning.unwrap_or_default()
            ))
        }
    }
}

fn area_exists(connection: &mut Connection, area: &str) -> Result<bool, String> {
    let (value, _) = connection.execute_function(&format!("AREA.EXIST(\"{area}\")"))?;
    Ok(value
        .trim_matches(|c: char| c == '(' || c == ')')
        .eq_ignore_ascii_case("true"))
}

/// Discard the data that is available in the pipe now. Gives up after 64 MiB,
/// e.g. if PowerView prints continuously.
fn drain_pipe(stream: &mut PipeStream, deadline: Instant) {
    let mut buf = vec![0u8; 64 * 1024];
    for _ in 0..1024 {
        if Instant::now() >= deadline {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// Whether PowerView closed the pipe. Discards the data that is available
/// now, like `drain_pipe`.
fn pipe_ended(stream: &mut PipeStream, deadline: Instant) -> bool {
    let mut buf = vec![0u8; 64 * 1024];
    for _ in 0..1024 {
        if Instant::now() >= deadline {
            return false;
        }
        match stream.read(&mut buf) {
            Ok(0) => return pipe_closed(stream),
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
    false
}

/// Select the piped AREA. Returns the previously selected AREA.
fn select_mcp_area(connection: &mut Connection) -> Result<String, String> {
    let selected = connection.execute_function("AREA.SELECTed()")?.0;
    if selected != MCP_AREA {
        connection.execute_command(&format!("AREA.Select {MCP_AREA}"))?;
    }
    Ok(selected)
}

/// Select `area` again.
fn restore_area(connection: &mut Connection, area: &str) {
    let target = if area.is_empty() { MCP_AREA } else { area };
    let current = connection
        .execute_function("AREA.SELECTed()")
        .map(|(value, _)| value)
        .unwrap_or_default();
    if current != target {
        let _ = connection.execute_command(&format!("AREA.Select {target}"));
    }
}

/// Error message if no instance could be connected.
fn connection_error(conn_errors: &[String]) -> String {
    if conn_errors.is_empty() {
        "Connection with TRACE32 could not be established".to_string()
    } else {
        format!(
            "Connection with TRACE32 could not be established: {}",
            conn_errors.join("; ")
        )
    }
}

/// Select `core` if the current core differs. CORE.Select is an unknown
/// command on single-core targets and before SYStem.CPU, so the only core
/// stays addressable then.
fn select_core(connection: &mut Connection, core: u32) -> Result<(), String> {
    let current = connection
        .execute_function("CORE()")
        .ok()
        .and_then(|(value, _)| value.trim_end_matches('.').parse::<u32>().ok());
    if current == Some(core) {
        return Ok(());
    }
    connection
        .execute_command(&format!("CORE.Select {core}."))
        .map_err(|e| format!("cannot select core {core}: {e}"))
}

/// Select `core` again if the current core differs. CORE.Select is an unknown
/// command on single-core targets: the failing command would halt a running
/// script and set ERROR.OCCURRED() for a paused one.
fn restore_core(connection: &mut Connection, core: u32) {
    let current = connection
        .execute_function("CORE()")
        .ok()
        .and_then(|(value, _)| value.trim_end_matches('.').parse::<u32>().ok());
    if current != Some(core) {
        let _ = connection.execute_command(&format!("CORE.Select {core}."));
    }
}

/// Terminate all scripts on the PRACTICE stack.
fn clear_practice_stack(connection: &mut Connection) -> Result<(), String> {
    for _ in 0..50 {
        connection.execute_command("END")?;
        thread::sleep(Duration::from_millis(50));
        let depth = connection.execute_function("PRACTICE.StackDepth()")?.0;
        if depth == "0." {
            return Ok(());
        }
    }
    Err(
        "Failed to clear PRACTICE stack. Another script is active and cannot be terminated."
            .to_string(),
    )
}

/// Describe the PRACTICE error since the last ERROR.RESet, if any.
fn error_of(connection: &mut Connection) -> Option<String> {
    let occurred = connection
        .execute_function("ERROR.OCCURRED()")
        .is_ok_and(|(value, _)| {
            value
                .trim_matches(|c: char| c == '(' || c == ')')
                .eq_ignore_ascii_case("true")
        });
    if !occurred {
        return None;
    }
    let message = connection
        .execute_function("ERROR.MESSAGE()")
        .map(|(value, _)| value)
        .unwrap_or_default();
    let cmdline = connection
        .execute_function("ERROR.CMDLINE()")
        .map(|(value, _)| value)
        .unwrap_or_default();
    let mut reason = format!(
        "Script stopped at an error and was terminated: {message}\nCommand: {}",
        cmdline.trim_start_matches("B::")
    );
    if let Some(frame) = call_stack(connection).first() {
        reason.push_str(&format!(
            "\nAt: {}:{}",
            frame["file"].as_str().unwrap_or(""),
            frame["line"]
        ));
    }
    Some(reason)
}

/// What to do if a PRACTICE script is on the stack when a new one starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IfBusy {
    /// Report the state and the call stack of the scripts.
    Fail,
    /// End all scripts, then run.
    Terminate,
    /// Run on top of a paused script, e.g. to operate its dialog.
    Nest,
}

impl IfBusy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "fail" => Some(Self::Fail),
            "terminate" => Some(Self::Terminate),
            "nest" => Some(Self::Nest),
            _ => None,
        }
    }
}

/// The PRACTICE call stack, innermost script first.
fn call_stack(connection: &mut Connection) -> Vec<Value> {
    let mut stack = Vec::new();
    for index in 0..32 {
        let Ok((file, _)) = connection.execute_function(&format!("PRACTICE.CALLER.FILE({index}.)"))
        else {
            break;
        };
        if file.is_empty() {
            break;
        }
        let line = connection
            .execute_function(&format!("PRACTICE.CALLER.LINE({index}.)"))
            .ok()
            .and_then(|(value, _)| value.trim_end_matches('.').parse::<u64>().ok())
            .unwrap_or(0);
        let text = connection
            .execute_function(&format!("PRACTICE.CALLER.TEXT({index}.)"))
            .map(|(value, _)| value.trim().to_string())
            .unwrap_or_default();
        stack.push(json!({ "file": file, "line": line, "text": text }));
    }
    stack
}

/// State of the PRACTICE interpreter: "idle", "running", "paused" (STOP,
/// e.g. waiting in a dialog) or "error" (halted at an error), with the call
/// stack and the open windows, which include named dialogs.
fn practice_state(connection: &mut Connection) -> Result<Value, String> {
    let depth = connection.execute_function("PRACTICE.StackDepth()")?.0;
    if depth == "0." {
        return Ok(json!({ "state": "idle" }));
    }
    let state = if connection.practice_running()? {
        "running"
    } else if connection
        .execute_function("ERROR.OCCURRED()")
        .is_ok_and(|(value, _)| {
            value
                .trim_matches(|c: char| c == '(' || c == ')')
                .eq_ignore_ascii_case("true")
        })
    {
        "error"
    } else {
        "paused"
    };
    let windows: Vec<String> = connection
        .execute_function("WINdow.LIST()")
        .map(|(value, _)| value)
        .unwrap_or_default()
        .split(',')
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty())
        .collect();
    let mut result = json!({
        "state": state,
        "stack": call_stack(connection),
        "windows": windows,
    });
    if state == "error" {
        let message = connection
            .execute_function("ERROR.MESSAGE()")
            .map(|(value, _)| value)
            .unwrap_or_default();
        result["error"] = json!(message);
    }
    Ok(result)
}

/// Describe a PRACTICE state from `practice_state` for a tool result.
fn describe_practice(state: &Value) -> String {
    let mut text = match state["state"].as_str() {
        Some("running") => "A PRACTICE script is running.".to_string(),
        Some("paused") => {
            "A PRACTICE script is paused (STOP) and waits, e.g. for input in a dialog.".to_string()
        }
        Some("error") => format!(
            "A PRACTICE script is halted at an error: {}",
            state["error"].as_str().unwrap_or("")
        ),
        _ => "No PRACTICE script is active.".to_string(),
    };
    if let Some(stack) = state["stack"].as_array() {
        text.push_str("\nCall stack, innermost first:");
        for frame in stack {
            text.push_str(&format!(
                "\n  {}:{}: {}",
                frame["file"].as_str().unwrap_or(""),
                frame["line"],
                frame["text"].as_str().unwrap_or("")
            ));
        }
    }
    if let Some(windows) = state["windows"].as_array() {
        let names: Vec<&str> = windows.iter().filter_map(|w| w.as_str()).collect();
        text.push_str(&format!(
            "\nOpen windows and named dialogs: {}",
            names.join(", ")
        ));
    }
    text
}

pub struct Worker {
    instances: Vec<PowerView>,
    require_explicit_target: bool,
}

pub type NativeOperation = Box<dyn FnOnce(u16) -> Result<Value, String> + Send>;

pub const WORKER_QUEUE_CAPACITY: usize = 16;

pub struct QueuedRequest {
    pub request: WorkerRequest,
    pub budget: Arc<RequestBudget>,
}

pub struct TemporaryScript {
    path: PathBuf,
}

impl TemporaryScript {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TemporaryScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub enum WorkerRequest {
    /// Local native observations share this worker with script operations.
    /// Unlike scripts, they never try another instance on connection failure.
    Native {
        port: Option<u16>,
        operation: NativeOperation,
        response_tx: mpsc::Sender<Result<Value, String>>,
    },
    ExecuteScript {
        script_path: String,
        temporary_script: Option<TemporaryScript>,
        args: String,
        /// Remote API port of the instance. None: the first reachable one.
        port: Option<u16>,
        /// TRACE32 core number (`CORE()`). None: the selected core.
        core: Option<u32>,
        timeout: Duration,
        if_busy: IfBusy,
        /// Device prompt selected before the script runs, e.g. "B::".
        device_prompt: String,
        response_tx: mpsc::Sender<Result<String, String>>,
    },
    RunCommand {
        command: String,
        /// Remote API port of the instance. None: the first reachable one.
        port: Option<u16>,
        /// TRACE32 core number (`CORE()`). None: the selected core.
        core: Option<u32>,
        /// Device prompt selected before the command, e.g. "B::".
        device_prompt: String,
        response_tx: mpsc::Sender<Result<(), String>>,
    },
    QueryTargetSystem {
        timeout: Duration,
        response_tx: mpsc::Sender<Result<serde_json::Value, String>>,
    },
    ManageInstances {
        action: InstanceAction,
        response_tx: mpsc::Sender<Result<serde_json::Value, String>>,
    },
}

impl WorkerRequest {
    fn reject(self, error: String) {
        match self {
            Self::Native { response_tx, .. }
            | Self::QueryTargetSystem { response_tx, .. }
            | Self::ManageInstances { response_tx, .. } => {
                let _ = response_tx.send(Err(error));
            }
            Self::ExecuteScript { response_tx, .. } => {
                let _ = response_tx.send(Err(error));
            }
            Self::RunCommand { response_tx, .. } => {
                let _ = response_tx.send(Err(error));
            }
        }
    }
}

/// Action on the PowerView instances the worker talks to.
pub enum InstanceAction {
    Inspect,
    /// Add the instance with this Remote API port.
    Add(u16),
    /// Remove the instance with this Remote API port.
    Remove(u16),
}

const NO_INSTANCE: &str = "No PowerView instance added. Add one with \
    trace32_manage_instances (action \"add\").";

impl Worker {
    /// Create a worker without contacting TRACE32.
    /// Connections will be established lazily on their first request.
    pub fn new(t32_ports: Vec<u16>) -> Self {
        Self {
            instances: t32_ports.into_iter().map(PowerView::new).collect(),
            require_explicit_target: false,
        }
    }

    pub fn with_explicit_target(mut self, required: bool) -> Self {
        self.require_explicit_target = required;
        self
    }

    /// The instances a request may run on, in order: the one with `port`,
    /// or all of them (the first reachable one is used).
    fn candidates(&self, port: Option<u16>) -> Result<Vec<usize>, String> {
        if self.instances.is_empty() {
            return Err(NO_INSTANCE.to_string());
        }
        match port {
            Some(port) => self
                .instances
                .iter()
                .position(|i| i.port == port)
                .map(|idx| vec![idx])
                .ok_or_else(|| {
                    format!(
                        "No instance with port {port} is added. Add it with \
                         trace32_manage_instances (action \"add\")."
                    )
                }),
            None => Ok((0..self.instances.len()).collect()),
        }
    }

    fn native_port(&self, port: Option<u16>) -> Result<u16, String> {
        let candidates = self.candidates(port)?;
        if candidates.len() != 1 {
            return Err(
                "Multiple PowerView instances are registered; native tools require an explicit port"
                    .to_string(),
            );
        }
        Ok(self.instances[candidates[0]].port)
    }

    fn execution_candidates(&self, port: Option<u16>) -> Result<Vec<usize>, String> {
        if self.require_explicit_target && port.is_none() && self.instances.len() > 1 {
            return Err(
                "Multiple PowerView instances are registered; --require-explicit-target requires an explicit port for script and command execution"
                    .to_string(),
            );
        }
        self.candidates(port)
    }

    /// Add or remove an instance, then report all instances with their
    /// connection state.
    fn manage_instances(
        &mut self,
        action: InstanceAction,
        deadline: Instant,
    ) -> Result<Value, String> {
        request_budget::remaining(deadline)?;
        match action {
            InstanceAction::Inspect => {}
            InstanceAction::Add(port) => {
                if self.instances.iter().any(|i| i.port == port) {
                    return Err(format!("An instance with port {port} is already added"));
                }
                self.instances.push(PowerView::new(port));
            }
            InstanceAction::Remove(port) => {
                let idx = self
                    .instances
                    .iter()
                    .position(|i| i.port == port)
                    .ok_or_else(|| format!("No instance with port {port} is added"))?;
                // Leave the instance without a pipe that nobody reads.
                self.instances.remove(idx).detach_pipe_before(deadline);
            }
        }

        let instances: Vec<Value> = self
            .instances
            .iter_mut()
            .map(|instance| match instance.ensure_rcl_connected(deadline) {
                Ok(()) => json!({ "port": instance.port, "state": "connected" }),
                Err(e) => json!({ "port": instance.port, "state": "not reachable", "error": e }),
            })
            .collect();
        Ok(json!({ "instances": instances }))
    }

    /// All instances in order, each with its connection state. Reachable
    /// ones also report their PRACTICE state and their cores. Fails only if
    /// no instance is reachable.
    fn query_target_system(
        &mut self,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.instances.is_empty() {
            return Err(NO_INSTANCE.to_string());
        }
        let mut instances = Vec::new();
        let mut conn_errors = Vec::new();
        for instance in &mut self.instances {
            let port = instance.port;
            if let Err(e) = instance.ensure_rcl_connected(deadline) {
                conn_errors.push(format!("port {port}: {e}"));
                instances.push(json!({ "port": port, "state": "not reachable", "error": e }));
                continue;
            }
            let mut entry = json!({ "port": port, "state": "connected" });
            let mut errors = Vec::new();
            // The PRACTICE state: an agent decides with it whether to wait
            // for, operate or end a script.
            match practice_state(instance.connection.as_mut().unwrap()) {
                Ok(practice) => entry["practice"] = practice,
                Err(e) => errors.push(format!("cannot read the PRACTICE state: {e}")),
            }
            match instance.query_instance(deadline, timeout) {
                Ok(cores) => entry["cores"] = Value::Array(cores),
                Err(e) => errors.push(format!("cannot query the cores: {e}")),
            }
            if !errors.is_empty() {
                entry["error"] = json!(errors.join("; "));
            }
            // Restore the default socket timeout for subsequent requests.
            let _ = instance
                .connection
                .as_mut()
                .unwrap()
                .set_timeout(Some(DEFAULT_TIMEOUT));
            instances.push(entry);
        }
        if conn_errors.len() == self.instances.len() {
            return Err(connection_error(&conn_errors));
        }
        Ok(json!({ "instances": instances }))
    }

    /// Serve requests until all senders are dropped, then detach the AREA
    /// output pipes.
    pub fn run(&mut self, rx: mpsc::Receiver<QueuedRequest>) {
        self.serve(rx);
        for instance in &mut self.instances {
            instance.detach_pipe();
        }
    }

    fn serve(&mut self, rx: mpsc::Receiver<QueuedRequest>) {
        for QueuedRequest { request, budget } in rx {
            if let Err(error) = budget.begin() {
                request.reject(error);
                continue;
            }
            let deadline = budget.deadline();
            match request {
                WorkerRequest::Native {
                    port,
                    operation,
                    response_tx,
                } => {
                    let result = self.native_port(port).and_then(|port| {
                        request_budget::remaining(deadline)?;
                        operation(port)
                    });
                    let _ = response_tx.send(result);
                }
                WorkerRequest::ExecuteScript {
                    script_path,
                    temporary_script: _temporary_script,
                    args,
                    port,
                    core,
                    timeout,
                    if_busy,
                    device_prompt,
                    response_tx,
                } => {
                    let result = (|| -> Result<String, String> {
                        let mut conn_errors = Vec::new();
                        for idx in self.execution_candidates(port)? {
                            request_budget::remaining(deadline)?;
                            let instance = &mut self.instances[idx];
                            match instance.ensure_connected(deadline) {
                                Ok(()) => {
                                    return instance.execute_script(
                                        script_path,
                                        args,
                                        core,
                                        &device_prompt,
                                        deadline,
                                        timeout,
                                        if_busy,
                                    );
                                }
                                Err(e) => conn_errors.push(format!("port {}: {e}", instance.port)),
                            }
                        }
                        Err(connection_error(&conn_errors))
                    })();
                    let _ = response_tx.send(result);
                }
                WorkerRequest::RunCommand {
                    command,
                    port,
                    core,
                    device_prompt,
                    response_tx,
                } => {
                    let result = (|| -> Result<(), String> {
                        let mut conn_errors = Vec::new();
                        for idx in self.execution_candidates(port)? {
                            request_budget::remaining(deadline)?;
                            let instance = &mut self.instances[idx];
                            match instance.ensure_connected(deadline) {
                                Ok(()) => {
                                    return instance.run_command(&command, core, &device_prompt);
                                }
                                Err(e) => conn_errors.push(format!("port {}: {e}", instance.port)),
                            }
                        }
                        Err(connection_error(&conn_errors))
                    })();
                    let _ = response_tx.send(result);
                }
                WorkerRequest::QueryTargetSystem {
                    timeout,
                    response_tx,
                } => {
                    let _ = response_tx.send(self.query_target_system(deadline, timeout));
                }
                WorkerRequest::ManageInstances {
                    action,
                    response_tx,
                } => {
                    let _ = response_tx.send(self.manage_instances(action, deadline));
                }
            }
        }
    }
}

#[cfg(test)]
mod native_worker_tests {
    use super::*;

    fn queued(request: WorkerRequest) -> QueuedRequest {
        QueuedRequest {
            request,
            budget: Arc::new(RequestBudget::new(Instant::now(), DEFAULT_TIMEOUT).unwrap()),
        }
    }

    #[test]
    fn execution_target_mode_preserves_legacy_and_single_instance_selection() {
        let worker = Worker::new(vec![20000, 20001]);
        assert_eq!(worker.execution_candidates(None).unwrap(), vec![0, 1]);
        let worker = Worker::new(vec![20000]).with_explicit_target(true);
        assert_eq!(worker.execution_candidates(None).unwrap(), vec![0]);
        assert_eq!(worker.execution_candidates(Some(20000)).unwrap(), vec![0]);
        assert!(worker.execution_candidates(Some(20001)).is_err());
        assert!(
            Worker::new(Vec::new())
                .with_explicit_target(true)
                .execution_candidates(None)
                .unwrap_err()
                .contains("No PowerView instance")
        );
    }

    #[test]
    fn execution_target_mode_uses_the_current_instance_registry_without_connecting() {
        let mut worker = Worker::new(vec![20000]).with_explicit_target(true);
        worker.instances.push(PowerView::new(20001));
        assert!(
            worker
                .execution_candidates(None)
                .unwrap_err()
                .contains("explicit port")
        );
        assert_eq!(worker.execution_candidates(Some(20001)).unwrap(), vec![1]);
        assert!(worker.execution_candidates(Some(20002)).is_err());
        worker.instances.remove(0);
        assert_eq!(worker.execution_candidates(None).unwrap(), vec![0]);
        assert_eq!(worker.instances[0].port, 20001);
        assert!(
            worker
                .instances
                .iter()
                .all(|instance| instance.connection.is_none())
        );
    }

    #[test]
    fn native_requires_an_unambiguous_registered_instance() {
        assert!(Worker::new(Vec::new()).native_port(None).is_err());
        let worker = Worker::new(vec![20000]);
        assert_eq!(worker.native_port(None).unwrap(), 20000);
        assert_eq!(worker.native_port(Some(20000)).unwrap(), 20000);
        assert!(worker.native_port(Some(20001)).is_err());
        let worker = Worker::new(vec![20000, 20001]);
        assert!(
            worker
                .native_port(None)
                .unwrap_err()
                .contains("explicit port")
        );
        assert_eq!(worker.native_port(Some(20001)).unwrap(), 20001);
    }

    #[test]
    fn native_requests_run_in_order_on_the_worker_thread() {
        let mut worker = Worker::new(vec![20000, 20001]);
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || worker.run(rx));
        let worker_thread = handle.thread().id();
        let mut responses = Vec::new();
        for (sequence, port) in [Some(20000), None, Some(20001)].into_iter().enumerate() {
            let (response_tx, response_rx) = mpsc::channel();
            responses.push(response_rx);
            tx.send(queued(WorkerRequest::Native {
                port,
                operation: Box::new(move |port| {
                    assert_eq!(thread::current().id(), worker_thread);
                    assert_ne!(sequence, 1, "Ambiguous selection must not run a closure");
                    Ok(json!({"sequence": sequence, "port": port}))
                }),
                response_tx,
            }))
            .unwrap();
        }
        drop(tx);
        assert_eq!(
            responses[0].recv().unwrap().unwrap(),
            json!({"sequence": 0, "port": 20000})
        );
        assert!(
            responses[1]
                .recv()
                .unwrap()
                .unwrap_err()
                .contains("explicit port")
        );
        assert_eq!(
            responses[2].recv().unwrap().unwrap(),
            json!({"sequence": 2, "port": 20001})
        );
        handle.join().unwrap();
    }

    #[test]
    fn expired_queued_operations_never_run_or_change_the_registry() {
        use std::sync::atomic::AtomicUsize;

        let mut worker = Worker::new(vec![20000]);
        let (tx, rx) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
        let handle = thread::spawn(move || {
            worker.serve(rx);
            worker
        });
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (first_tx, first_rx) = mpsc::channel();
        tx.send(queued(WorkerRequest::Native {
            port: None,
            operation: Box::new(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(Value::Null)
            }),
            response_tx: first_tx,
        }))
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let operation_calls = Arc::clone(&calls);
        let (second_tx, second_rx) = mpsc::channel();
        let second = queued(WorkerRequest::Native {
            port: None,
            operation: Box::new(move |_| {
                operation_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Null)
            }),
            response_tx: second_tx,
        });
        let second_budget = Arc::clone(&second.budget);
        tx.send(second).unwrap();
        assert!(second_budget.expire().contains("not_started"));

        let (manage_tx, manage_rx) = mpsc::channel();
        tx.send(QueuedRequest {
            request: WorkerRequest::ManageInstances {
                action: InstanceAction::Add(20001),
                response_tx: manage_tx,
            },
            budget: Arc::new(RequestBudget::new(Instant::now(), Duration::ZERO).unwrap()),
        })
        .unwrap();
        release_tx.send(()).unwrap();
        drop(tx);
        first_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        for response in [second_rx, manage_rx] {
            assert!(
                response
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap_err()
                    .contains("not_started")
            );
        }
        let worker = handle.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(worker.instances.len(), 1);
        assert_eq!(worker.instances[0].port, 20000);
        assert!(worker.instances[0].connection.is_none());
    }

    #[test]
    fn timing_out_started_work_does_not_cancel_or_retry_it() {
        use std::sync::atomic::AtomicUsize;

        let mut worker = Worker::new(vec![20000]);
        let (tx, rx) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
        let handle = thread::spawn(move || worker.serve(rx));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let operation_calls = Arc::clone(&calls);
        let request = queued(WorkerRequest::Native {
            port: None,
            operation: Box::new(move |_| {
                operation_calls.fetch_add(1, Ordering::SeqCst);
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(Value::Null)
            }),
            response_tx,
        });
        let budget = Arc::clone(&request.budget);
        tx.send(request).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(budget.expire().contains("unknown_completion"));
        drop(response_rx);
        release_tx.send(()).unwrap();
        drop(tx);
        handle.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn queued_temporary_scripts_survive_caller_expiry_and_are_cleaned_on_rejection() {
        let path = std::env::temp_dir().join(format!(
            "t32mcp-owned-script-{}-{}.cmm",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::write(&path, "ENDDO\n").unwrap();
        let mut worker = Worker::new(vec![20000]);
        let (tx, rx) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (first_tx, _first_rx) = mpsc::channel();
        let handle = thread::spawn(move || worker.serve(rx));
        tx.send(queued(WorkerRequest::Native {
            port: None,
            operation: Box::new(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(Value::Null)
            }),
            response_tx: first_tx,
        }))
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (response_tx, response_rx) = mpsc::channel();
        let request = queued(WorkerRequest::ExecuteScript {
            script_path: path.to_string_lossy().into_owned(),
            temporary_script: Some(TemporaryScript::new(path.clone())),
            args: String::new(),
            port: None,
            core: None,
            timeout: DEFAULT_TIMEOUT,
            if_busy: IfBusy::Fail,
            device_prompt: "B::".to_string(),
            response_tx,
        });
        let budget = Arc::clone(&request.budget);
        tx.send(request).unwrap();
        assert!(budget.expire().contains("not_started"));
        assert!(path.exists(), "a queued job must still own its script");
        drop(response_rx);
        release_tx.send(()).unwrap();
        drop(tx);
        handle.join().unwrap();
        assert!(!path.exists(), "rejected jobs must clean up their scripts");
    }
}
