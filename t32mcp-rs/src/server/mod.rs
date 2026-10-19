// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

mod native;
mod output;
mod practice;

use crate::cli::T32Options;
use output::T32Sink;
use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool_handler,
};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use t32mcp::{t32_cmd, t32_connect, t32_disconnect, t32_fnc};
use tokio::sync::Mutex;

static SINK: LazyLock<Mutex<T32Sink>> = LazyLock::new(|| Mutex::new(T32Sink::new()));
static CONNECTED: AtomicBool = AtomicBool::new(false);

// Keep one shared channel on a fixed thread. Vendor builds may use either
// global or thread-local state; the library also serializes individual calls.
static OPERATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Clone)]
pub(crate) struct T32 {
    tool_router: ToolRouter<Self>,
    options: T32Options,
}

impl T32 {
    pub(crate) fn new(options: T32Options) -> Self {
        let mut tool_router = Self::collection_router();
        if !options.read_only {
            tool_router += Self::practice_router();
            if options.unlock_all_tools {
                tool_router += Self::raw_practice_router();
            }
        }
        if options.native_tools {
            tool_router += Self::native_router();
        }
        Self {
            tool_router,
            options,
        }
    }

    fn mark_connection_failed(&self) {
        CONNECTED.store(false, Ordering::Relaxed);
        t32_disconnect();
        if let Ok(mut sink) = SINK.try_lock() {
            sink.reset();
        }
    }

    fn checked_cmd(&self, command: String) -> Result<(), String> {
        t32_cmd(command).inspect_err(|_| self.mark_connection_failed())
    }

    fn checked_fnc(&self, expression: String) -> Result<String, String> {
        t32_fnc(expression).inspect_err(|_| self.mark_connection_failed())
    }

    fn empty_stack(&self) -> Result<bool, String> {
        let depth = self.checked_fnc("PRACTICE.StackDepth()".to_string())?;
        let is_empty = matches!(depth.trim(), "0." | "0" | "0x0");
        Ok(is_empty)
    }

    fn establish_debug_connection(&self, ignore_stack: bool) -> Result<(), String> {
        if !CONNECTED.load(Ordering::Relaxed) {
            t32_connect(self.options.port).inspect_err(|_| self.mark_connection_failed())?;
            CONNECTED.store(true, Ordering::Relaxed);
        }

        let empty = match self.empty_stack() {
            Ok(empty) => empty,
            Err(_) => {
                t32_connect(self.options.port).inspect_err(|_| self.mark_connection_failed())?;
                CONNECTED.store(true, Ordering::Relaxed);
                self.empty_stack()?
            }
        };
        if !empty && !ignore_stack {
            return Err("Discovered that the PRACTICE stack depth is greater than 0, indicating that another script is active. To continue, the running script must first finish execution. We do not allow multiple scripts to run simultaneously.".to_string());
        }
        Ok(())
    }

    fn require_write_access(&self) -> Result<(), String> {
        if self.options.read_only {
            return Err("This tool is disabled by --read-only".to_string());
        }
        Ok(())
    }

    fn ensure_inspection_connection_once(&self) -> Result<(), String> {
        if !CONNECTED.load(Ordering::Relaxed) {
            t32_connect(self.options.port)?;
            CONNECTED.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for T32 {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder().enable_tools().build();
        let info = Implementation::new(
            env!("CARGO_CRATE_NAME").to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        )
        .with_title("Lauterbach TRACE32 MCP Server")
        .with_website_url("https://gitlab.com/lauterbach/t32mcp");

        ServerInfo::new(capabilities).with_server_info(info)
    }
}

pub(crate) async fn shutdown() {
    SINK.lock().await.reset();
    t32_disconnect();
    CONNECTED.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    const NATIVE_TOOLS: [&str; 14] = [
        "capture_snapshot",
        "diff_snapshots",
        "doctor",
        "evaluate_expressions",
        "get_debugger_status",
        "list_practice_skills",
        "lookup_symbols",
        "read_debugger_window",
        "read_memory",
        "read_per_registers",
        "read_per_views",
        "read_registers",
        "verify_image",
        "wait_for_condition",
    ];

    fn options(native_tools: bool, read_only: bool, unlock_all_tools: bool) -> T32Options {
        T32Options {
            native_tools,
            target_config: t32mcp::capabilities::TargetConfig::default(),
            unlock_all_tools,
            read_only,
            skills_base: "unused".to_string(),
            artifacts_base: PathBuf::new(),
            port: 20000,
            pipe_name: "test-unused".to_string(),
        }
    }

    fn tools(server: &T32) -> BTreeSet<String> {
        server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    #[test]
    fn default_router_contains_exactly_the_three_generic_tools() {
        let server = T32::new(options(false, false, false));
        let expected = BTreeSet::from([
            "abort_practice_skill".to_string(),
            "collect_practice_skill_response".to_string(),
            "execute_practice_skill".to_string(),
        ]);
        assert_eq!(tools(&server), expected);
    }

    #[test]
    fn router_mode_matrix_preserves_explicit_opt_in_and_read_only() {
        for (native_tools, read_only, unlock_all_tools) in [
            (false, false, false),
            (false, false, true),
            (false, true, false),
            (false, true, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ] {
            let server = T32::new(options(native_tools, read_only, unlock_all_tools));
            let mut expected = BTreeSet::from(["collect_practice_skill_response".to_string()]);
            if !read_only {
                expected.insert("execute_practice_skill".to_string());
                expected.insert("abort_practice_skill".to_string());
                if unlock_all_tools {
                    expected.insert("execute_practice".to_string());
                }
            }
            if native_tools {
                expected.extend(NATIVE_TOOLS.iter().map(|name| name.to_string()));
            }
            assert_eq!(
                tools(&server),
                expected,
                "native_tools={native_tools}, read_only={read_only}, t32chat={unlock_all_tools}"
            );
            for name in NATIVE_TOOLS {
                assert_eq!(server.tool_router.has_route(name), native_tools);
            }
            assert_eq!(
                server.tool_router.has_route("execute_practice"),
                unlock_all_tools && !read_only
            );
            assert_eq!(
                server.tool_router.has_route("execute_practice_skill"),
                !read_only
            );
            assert_eq!(
                server.tool_router.has_route("abort_practice_skill"),
                !read_only
            );
        }
    }
}
