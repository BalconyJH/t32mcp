// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Rust port of the parts of the TRACE32 Remote API (RCL) that are needed
//! by this MCP server.
//!
//! Only the TCP transport (NETTCP configuration) is implemented.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const RAPI_CMD_ATTACH: u8 = 0x71;
const RAPI_CMD_EXECUTE_PRACTICE: u8 = 0x72;

const PRACTICE_EXECUTECOMMAND: u8 = 0x04;
const PRACTICE_EXECUTEFUNCTION: u8 = 0x05;

const T32_ERR_OK: u8 = 0x00;

const T32_NETTCP_RCL_REQ: u32 = 0x0010;
const T32_NETTCP_RCL_RESP: u32 = 0x0011;
const T32_NETTCP_RCL_NOTIFY: u32 = 0x0012;

const T32_API_KEEPALIVE: u8 = 0xFE;

const HEADER_LEN: usize = 8;
const ALIGN64: usize = 8;

/// Maximum receive buffer size
const PACKLEN: usize = 0x4000;

/// Default response buffer size
const RESPONSE_BUFFER_SIZE: u32 = 4096;

pub const T32_DEV_ICD: u32 = 1;

fn align_up(n: usize, m: usize) -> usize {
    if n.is_multiple_of(m) { 0 } else { m - (n % m) }
}

/// A TCP connection to the TRACE32 Remote API of a PowerView instance.
pub struct Connection {
    socket: TcpStream,
    socket_content: Vec<u8>,
    response_queue: VecDeque<Vec<u8>>,
    /// Notification messages are not used by this client, but must be
    /// separated from responses to keep the stream in sync.
    notify_queue: VecDeque<Vec<u8>>,
    message_id: u32,
}

impl Connection {
    pub fn connect(node: &str, port: u16, timeout: Option<Duration>) -> Result<Self, String> {
        let addr = (node, port)
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {node}:{port}: {e}"))?
            .next()
            .ok_or_else(|| format!("cannot resolve {node}:{port}"))?;

        let socket = match timeout {
            Some(t) => TcpStream::connect_timeout(&addr, t),
            None => TcpStream::connect(addr),
        }
        .map_err(|e| format!("cannot connect to {addr}: {e}"))?;

        socket
            .set_read_timeout(timeout)
            .map_err(|e| format!("cannot set read timeout: {e}"))?;
        socket
            .set_write_timeout(timeout)
            .map_err(|e| format!("cannot set write timeout: {e}"))?;
        let _ = socket.set_nodelay(true);

        Ok(Connection {
            socket,
            socket_content: Vec::new(),
            response_queue: VecDeque::new(),
            notify_queue: VecDeque::new(),
            message_id: 0,
        })
    }

    fn current_message_id(&self) -> u8 {
        (self.message_id % 255) as u8
    }

    fn increment_message_id(&mut self) -> u8 {
        self.message_id += 1;
        self.current_message_id()
    }

    fn transmit(&mut self, data: &[u8]) -> Result<(), String> {
        let size = data.len();
        let padding = align_up(size + HEADER_LEN, ALIGN64);

        let mut buf = Vec::with_capacity(HEADER_LEN + size + padding);
        buf.extend_from_slice(&(size as u32).to_le_bytes());
        buf.extend_from_slice(&T32_NETTCP_RCL_REQ.to_le_bytes());
        buf.extend_from_slice(data);
        buf.resize(HEADER_LEN + size + padding, 0);

        self.socket
            .write_all(&buf)
            .map_err(|e| format!("transmit failed: {e}"))
    }

    fn extract_messages(&mut self) -> Result<(), String> {
        loop {
            if self.socket_content.len() < HEADER_LEN {
                break;
            }
            let payload_len = u32::from_le_bytes(
                self.socket_content[0..4]
                    .try_into()
                    .expect("slice of length 4"),
            ) as usize;
            let message_len = payload_len + HEADER_LEN;
            if self.socket_content.len() < message_len {
                break;
            }
            let message_type = u32::from_le_bytes(
                self.socket_content[4..8]
                    .try_into()
                    .expect("slice of length 4"),
            );
            let payload = self.socket_content[HEADER_LEN..message_len].to_vec();

            // TRACE32 aligns messages to 64 bit.
            let stuffed = message_len + align_up(message_len, ALIGN64);
            let drain_to = stuffed.min(self.socket_content.len());
            self.socket_content.drain(..drain_to);

            match message_type {
                T32_NETTCP_RCL_NOTIFY => self.notify_queue.push_back(payload),
                T32_NETTCP_RCL_RESP => self.response_queue.push_back(payload),
                t => return Err(format!("TCP packet contains invalid message type {t:#x}")),
            }
        }
        Ok(())
    }

    fn poll_response(&mut self) -> Result<(), String> {
        let mut buf = [0u8; PACKLEN];
        let n = self
            .socket
            .read(&mut buf)
            .map_err(|e| format!("receive failed: {e}"))?;
        if n == 0 {
            return Err("connection closed by peer".to_string());
        }
        self.socket_content.extend_from_slice(&buf[..n]);
        self.extract_messages()
    }

    fn receive(&mut self) -> Result<Vec<u8>, String> {
        let message_id = self.current_message_id();
        loop {
            match self.response_queue.pop_front() {
                Some(message) => {
                    if message.len() < 2 {
                        continue;
                    }
                    if message[0] == T32_API_KEEPALIVE {
                        continue;
                    }
                    if message[1] < message_id {
                        continue;
                    }
                    if message[1] == message_id {
                        return Ok(message);
                    }
                    return Err("messages out of sync, connection broken".to_string());
                }
                None => self.poll_response()?,
            }
        }
    }

    fn generic_api_call(
        &mut self,
        rapi_cmd: u8,
        opt_arg: u8,
        payload: &[u8],
        sendonly: bool,
        force_length: Option<usize>,
    ) -> Result<Vec<u8>, String> {
        let payload_len = payload.len();
        let mut msg_len = force_length.unwrap_or(2 + payload_len);

        let mut send_data: Vec<u8> = Vec::with_capacity(6 + payload_len + 1);
        if msg_len > 0xFF {
            msg_len += 2;
            if msg_len > 0xF000 {
                return Err("message buffer too large".to_string());
            }
            let id = self.increment_message_id();
            send_data.extend_from_slice(&[0, rapi_cmd, opt_arg, id]);
            send_data.extend_from_slice(&(msg_len as u16).to_le_bytes());
        } else {
            let id = self.increment_message_id();
            send_data.extend_from_slice(&[msg_len as u8, rapi_cmd, opt_arg, id]);
        }
        send_data.extend_from_slice(payload);
        // pad payload to even length
        send_data.resize(send_data.len() + payload_len % 2, 0);

        self.transmit(&send_data)?;

        if sendonly {
            return Ok(Vec::new());
        }

        let recv = self.receive()?;
        if recv.is_empty() {
            return Err("empty response received".to_string());
        }
        if recv[0] != T32_ERR_OK {
            let mut msg = None;
            if recv.len() > 10 {
                let len =
                    u32::from_le_bytes(recv[6..10].try_into().expect("slice of length 4")) as usize;
                let end = (10 + len).min(recv.len());
                let text = String::from_utf8_lossy(&recv[10..end])
                    .trim_matches('\0')
                    .to_string();
                if !text.is_empty() {
                    msg = Some(text);
                }
            }
            return Err(match msg {
                Some(m) => format!("TRACE32 error {}: {m}", recv[0]),
                None => format!("TRACE32 error {}", recv[0]),
            });
        }

        Ok(recv[2..].to_vec())
    }

    /// `T32_Attach`
    pub fn attach(&mut self, device_specifier: u8) -> Result<(), String> {
        self.generic_api_call(RAPI_CMD_ATTACH, device_specifier, &[], false, None)?;
        Ok(())
    }

    /// `T32_ExecuteCommand`: execute a PRACTICE command,
    /// error messages are reported back by PowerView.
    pub fn execute_command(&mut self, command: &str) -> Result<(), String> {
        let mut payload = RESPONSE_BUFFER_SIZE.to_le_bytes().to_vec();
        payload.extend_from_slice(command.as_bytes());
        payload.push(0);
        self.generic_api_call(
            RAPI_CMD_EXECUTE_PRACTICE,
            PRACTICE_EXECUTECOMMAND,
            &payload,
            false,
            None,
        )?;
        Ok(())
    }

    /// `T32_ExecuteFunction`: evaluate a PRACTICE function.
    ///
    /// Returns the result string and the TRACE32 result type.
    pub fn execute_function(&mut self, function: &str) -> Result<(String, u32), String> {
        let mut payload = RESPONSE_BUFFER_SIZE.to_le_bytes().to_vec();
        payload.extend_from_slice(function.as_bytes());
        payload.push(0);

        let result = self.generic_api_call(
            RAPI_CMD_EXECUTE_PRACTICE,
            PRACTICE_EXECUTEFUNCTION,
            &payload,
            false,
            None,
        )?;

        if result.len() < 8 {
            return Err("invalid function response".to_string());
        }
        let result_type = u32::from_le_bytes(result[0..4].try_into().expect("slice of length 4"));
        let result_size =
            u32::from_le_bytes(result[4..8].try_into().expect("slice of length 4")) as usize;
        let end = (8 + result_size).min(result.len());
        let value = String::from_utf8_lossy(&result[8..end])
            .trim_end_matches('\0')
            .to_string();

        Ok((value, result_type))
    }
}
