// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use super::{CONNECTED, OPERATION_LOCK, SINK, T32, output::MAX_OUTPUT_BYTES};
use crate::skill_paths;
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, DefaultHasher};
use std::io::Write;
use std::sync::atomic::Ordering;
use t32mcp::{t32_cmd, t32_open_pipe};
use tempfile::Builder;
use tokio::{
    io::{AsyncReadExt, BufReader},
    time::{Duration, sleep, timeout},
};

const PIPE_ACCEPT_TIMEOUT: Duration = Duration::from_secs(3);
const PIPE_DRAIN_POLLS: usize = 5;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SkillRequest {
    #[schemars(
        description = "Name of the skill containing the script: .../skill_name/scripts/script_name"
    )]
    pub skill_name: String,

    #[schemars(description = "Name of the PRACTICE script file (*.cmm) to execute.")]
    pub script_name: String,

    #[schemars(description = "Arguments to pass to the script")]
    pub script_args: Option<HashMap<String, String, BuildHasherDefault<DefaultHasher>>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PracticeRequest {
    #[schemars(description = "Content of the PRACTICE script.")]
    pub practice: String,

    #[schemars(description = "Value indicating whether to execute the PRACTICE script.")]
    pub execute: bool,
}

impl T32 {
    async fn reset_pipe(&self, close_area: bool) {
        SINK.lock().await.reset();
        if close_area && CONNECTED.load(Ordering::Relaxed) {
            let _ = self.checked_cmd("AREA.CLOSE".to_string());
        }
    }

    async fn collect_response_unlocked(&self) -> Result<String, String> {
        sleep(Duration::from_millis(100)).await;
        let active = SINK.lock().await.active;
        if active && self.empty_stack()? {
            self.checked_cmd("AREA.CLOSE".to_string())?;
            for _ in 0..PIPE_DRAIN_POLLS {
                if !SINK.lock().await.active {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
            let mut sink = SINK.lock().await;
            if sink.active {
                // Never let a stalled old reader append into the next request.
                sink.truncated = true;
                sink.stop_reader();
            }
        }
        Ok(SINK.lock().await.response())
    }
}

#[tool_router(router = practice_router, vis = "pub(super)")]
impl T32 {
    #[tool(
        name = "execute_practice_skill",
        description = "Execute a PRACTICE script from a specified skill using TRACE32 PowerView.
This tool starts the execution of a TRACE32 PRACTICE script (.cmm file)
located within a skill's scripts directory.
The script is run on a connected TRACE32 PowerView instance via the API Port.

We do not allow the execution of multiple scripts simultaneously. To ensure that,
we check the PRACTICE stack depth before the launch of the script.

It is possible that the executed script contains an error.
In that case, this function does NOT return an error since it only initiates
the execution and does not wait for completion. Script errors can only be observed
via the PowerView GUI by the user, or inferred if the script is still running
during the next tool call.

This tool returns the initial output with <FINISHED> or <NOT FINISHED>.
If not finished, use collect_practice_skill_response to poll for more output.

Returns: Result of the operation.

Possible exceptions:
    - The specified (skill_name, script_name) is not found
    - TRACE32 PowerView is not running or the API Port is not enabled
    - Another script is already running

Prerequisites:
    - TRACE32 PowerView must be running
    - API Port must be enabled in PowerView configuration
    - The specified skill and script must exist in the expected directory structure"
    )]
    async fn execute_practice_skill(
        &self,
        Parameters(SkillRequest {
            skill_name,
            script_name,
            script_args,
        }): Parameters<SkillRequest>,
    ) -> Result<String, String> {
        let _operation = OPERATION_LOCK.lock().await;
        self.require_write_access()?;
        let path =
            skill_paths::resolve_script(&self.options.skills_base, &skill_name, &script_name)?;
        let command = skill_paths::do_command(&path, script_args.as_ref())?;
        if SINK.lock().await.active {
            return Err(
                "A skill output pipe is still active; collect its response or abort it first"
                    .to_string(),
            );
        }
        self.establish_debug_connection(false)?;

        // This bounds asynchronous pipe acceptance only, not synchronous native
        // Remote API calls executed before the pipe's accept future is polled.
        let connection = match timeout(PIPE_ACCEPT_TIMEOUT, t32_open_pipe(&self.options.pipe_name))
            .await
        {
            Ok(Ok(connection)) => connection,
            Ok(Err(error)) => {
                self.reset_pipe(true).await;
                self.mark_connection_failed();
                return Err(error);
            }
            Err(_) => {
                self.reset_pipe(true).await;
                return Err(
                    "Output pipe acceptance timed out after 3 seconds; synchronous Remote API calls are not time-limited"
                        .to_string(),
                );
            }
        };

        let generation = SINK.lock().await.activate();
        if let Err(error) = t32_cmd(command) {
            self.reset_pipe(true).await;
            self.mark_connection_failed();
            return Err(error);
        }

        let reader = tokio::spawn(async move {
            let mut receiver = BufReader::with_capacity(256 * 1024, connection);
            let mut buf = vec![0u8; 64 * 1024];

            loop {
                let res = receiver.read(&mut buf).await;
                let mut sink = SINK.lock().await;
                if sink.generation != generation {
                    break;
                }
                match res {
                    Ok(0) => {
                        sink.active = false;
                        break;
                    }
                    Ok(n) => {
                        let message = String::from_utf8_lossy(&buf[..n]);
                        sink.append(&message);
                    }
                    Err(e) => {
                        sink.active = false;
                        sink.append(&format!("\nRead error: {e}"));
                        break;
                    }
                }
            }
        });
        SINK.lock().await.reader = Some(reader);

        self.collect_response_unlocked().await
    }

    #[tool(
        name = "abort_practice_skill",
        description = "Aborts the execution of a running PRACTICE skill.

        Use this tool when you are certain that the PRACTICE skill should have terminated by now.
        Communicate this with the user and hint at the PLIST command that can be used in the GUI
        to show the currently active script."
    )]
    async fn abort_practice_skill(&self) -> Result<(), String> {
        let _operation = OPERATION_LOCK.lock().await;
        self.require_write_access()?;
        let had_pipe = SINK.lock().await.active;
        let result = self
            .establish_debug_connection(true)
            .and_then(|()| self.checked_cmd("END".to_string()));
        self.reset_pipe(had_pipe).await;
        result
    }
}

#[tool_router(router = collection_router, vis = "pub(super)")]
impl T32 {
    #[tool(
        name = "collect_practice_skill_response",
        description = "Collects the response generated so far by a previous call to execute_practice_skill.

        It is possible that the script has not finished execution yet.
        To communicate this information, the returned string starts with <[NOT] FINISHED>.

Returns: Generated output by a previously called script. String begins either finished <FINISHED> or <NOT FINSIHED>,
indicating whether the the script is still active. If the output of a finished script has been collected,
the internal buffer will be cleared until the next tool call of collect_practice_skill_response.

Prerequisites:
    - TRACE32 PowerView must be running
    - API Port must be enabled in PowerView configuration
    - execute_practice_skill must have been called.

Important: Only call this tool after a execute_practice_skill that has NOT finished execution (indicated by its result)."
    )]
    async fn collect_practice_skill_response(&self) -> Result<String, String> {
        let _operation = OPERATION_LOCK.lock().await;
        self.collect_response_unlocked().await
    }
}

#[tool_router(router = raw_practice_router, vis = "pub(super)")]
impl T32 {
    #[tool(
        name = "execute_practice",
        description = "Execute PRACTICE commands via Remote API"
    )]
    async fn execute_practice(
        &self,
        Parameters(PracticeRequest { practice, execute }): Parameters<PracticeRequest>,
    ) -> Result<(), String> {
        let _operation = OPERATION_LOCK.lock().await;
        self.require_write_access()?;
        validate_practice(&practice)?;
        // Establish the debug connection first to avoid the creation of a temp file without feedback
        self.establish_debug_connection(false)?;

        let practice = practice.trim();
        let cmd = if !execute || practice.contains("\n") || practice.contains(";") {
            // PRACTICE script will always be executed as files
            // Comments cannot be passed to the command line

            let mut script = Builder::new()
                .prefix("t32chat_")
                .suffix(".cmm")
                .tempfile()
                .map_err(|e| format!("Temp file creation failed ({e})"))?;
            script
                .write_all(practice.as_bytes())
                .and_then(|()| script.flush())
                .map_err(|e| format!("Script file write failed ({e})"))?;

            let op = if execute { "DO" } else { "PEDIT" };
            let path = script
                .path()
                .to_str()
                .ok_or_else(|| "Temporary script path is not valid Unicode".to_string())?;
            let command = format!(
                "{op} {}",
                skill_paths::quote_practice_literal(path, "Temporary script path")?
            );
            // DO and PEDIT can use the file after this tool has returned.
            script
                .keep()
                .map_err(|e| format!("Cannot retain temporary script ({e})"))?;
            command
        } else {
            practice.to_string()
        };

        self.checked_cmd(cmd)?;

        Ok(())
    }
}

fn validate_practice(practice: &str) -> Result<(), String> {
    if practice.trim().is_empty() {
        return Err("PRACTICE content cannot be empty".to_string());
    }
    if practice.len() > MAX_OUTPUT_BYTES {
        return Err("PRACTICE content exceeds the 1 MiB limit".to_string());
    }
    if practice
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err("PRACTICE content contains a prohibited control character".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn practice_validation_rejects_nul_and_keeps_multiline_scripts() {
        assert!(validate_practice("PRINT \"ok\"\nEND").is_ok());
        assert!(validate_practice(" \r\n ").is_err());
        assert!(validate_practice("PRINT\0END").is_err());
        assert!(validate_practice(&"x".repeat(MAX_OUTPUT_BYTES + 1)).is_err());
    }
}
