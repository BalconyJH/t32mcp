// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Tool handlers of t32mcp and the infrastructure they share: the context,
//! the tool entry, and the helpers that talk to the worker thread.
//! Each tool handler is expected to be implemented in a separate file that
//! provides an `entry` function (parameterizable)

pub mod native;
pub mod trace32_get_status;
pub mod trace32_manage_instances;
pub mod trace32_run_command;
pub mod trace32_run_script;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::execution_policy;
use crate::mcp;
use crate::t32;

pub type ToolHandler = fn(Option<&serde_json::Value>, &mut Context) -> mcp::CallToolResult;

pub struct ToolEntry {
    pub tool: mcp::Tool,
    pub handler: ToolHandler,
}

pub struct Context {
    pub event_queue: VecDeque<serde_json::Value>,
    pub instructions: String,
    pub policy: execution_policy::ExecutionPolicy,
    /// --skills: if not empty, skill scripts must lie below one of them.
    pub skill_roots: Vec<PathBuf>,
    /// Local opt-in restrictions and native inspection configuration.
    pub read_only: bool,
    pub native: Option<native::NativeOptions>,
    pub request_started: Instant,
    pub request_timeout: Duration,
    pub tools: Vec<ToolEntry>,
    pub worker_tx: mpsc::SyncSender<t32::QueuedRequest>,
}

pub fn error_result(text: &str) -> mcp::CallToolResult {
    call_result(text.to_string(), Some(true), None)
}

pub fn text_result(text: &str) -> mcp::CallToolResult {
    call_result(text.to_string(), None, None)
}

fn call_result(
    text: String,
    is_error: Option<bool>,
    structured_content: Option<serde_json::Value>,
) -> mcp::CallToolResult {
    mcp::CallToolResult {
        content: vec![mcp::ContentBlock::TextContent(mcp::TextContent {
            annotations: None,
            meta: None,
            text,
            type_: "text".to_string(),
        })],
        is_error,
        meta: None,
        result_type: "complete".to_string(),
        structured_content,
    }
}

/// Sends a request to the worker thread and waits for its response.
fn worker_call<R>(
    ctx: &Context,
    timeout: Duration,
    make_request: impl FnOnce(mpsc::Sender<R>) -> t32::WorkerRequest,
) -> Result<R, String> {
    let budget = Arc::new(crate::request_budget::RequestBudget::new(
        ctx.request_started,
        timeout,
    )?);
    if budget.remaining().is_zero() {
        return Err(budget.expire());
    }
    let (tx, rx) = mpsc::channel();
    let request = t32::QueuedRequest {
        request: make_request(tx),
        budget: Arc::clone(&budget),
    };
    ctx.worker_tx
        .try_send(request)
        .map_err(|error| match error {
            mpsc::TrySendError::Full(_) => {
                "Worker queue is full (not_started): no operation was submitted".to_string()
            }
            mpsc::TrySendError::Disconnected(_) => "Worker thread is not available".to_string(),
        })?;
    match rx.recv_timeout(budget.remaining()) {
        Ok(_) if budget.remaining().is_zero() => Err(budget.expire()),
        Ok(response) => Ok(response),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(budget.expire()),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("Worker thread dropped the response channel".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn context() -> (Context, mpsc::Receiver<t32::QueuedRequest>) {
        let (worker_tx, worker_rx) = mpsc::sync_channel(1);
        (
            Context {
                event_queue: VecDeque::new(),
                instructions: String::new(),
                policy: execution_policy::ExecutionPolicy::Unrestricted,
                skill_roots: Vec::new(),
                read_only: false,
                native: None,
                request_started: Instant::now(),
                request_timeout: Duration::from_secs(60),
                tools: Vec::new(),
                worker_tx,
            },
            worker_rx,
        )
    }

    fn status_request(
        response_tx: mpsc::Sender<Result<serde_json::Value, String>>,
    ) -> t32::WorkerRequest {
        t32::WorkerRequest::QueryTargetSystem {
            timeout: Duration::from_secs(60),
            response_tx,
        }
    }

    fn fill_queue(ctx: &Context) {
        let (response_tx, _) = mpsc::channel();
        assert!(
            ctx.worker_tx
                .try_send(t32::QueuedRequest {
                    request: status_request(response_tx),
                    budget: Arc::new(
                        crate::request_budget::RequestBudget::new(
                            Instant::now(),
                            Duration::from_secs(60),
                        )
                        .unwrap(),
                    ),
                })
                .is_ok()
        );
    }

    fn temporary_script() -> (PathBuf, t32::TemporaryScript) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "t32mcp-worker-budget-test-{}-{}.cmm",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::write(&path, "ENDDO\n").unwrap();
        let guard = t32::TemporaryScript::new(path.clone());
        (path, guard)
    }

    fn script_request(
        path: &std::path::Path,
        script: t32::TemporaryScript,
        response_tx: mpsc::Sender<Result<String, String>>,
    ) -> t32::WorkerRequest {
        t32::WorkerRequest::ExecuteScript {
            script_path: path.to_string_lossy().into_owned(),
            temporary_script: Some(script),
            args: String::new(),
            port: None,
            core: None,
            timeout: Duration::from_secs(60),
            if_busy: t32::IfBusy::Fail,
            device_prompt: "B::".to_string(),
            response_tx,
        }
    }

    #[test]
    fn expired_requests_are_not_constructed_or_enqueued() {
        let (mut ctx, rx) = context();
        ctx.request_started = Instant::now() - Duration::from_secs(1);
        let result = worker_call::<()>(&ctx, Duration::from_millis(1), |_| {
            panic!("expired requests must not be constructed")
        });
        assert!(result.unwrap_err().contains("not_started"));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn saturated_worker_queue_rejects_without_blocking_or_extra_entries() {
        let (ctx, rx) = context();
        fill_queue(&ctx);
        let error = worker_call(&ctx, Duration::from_secs(60), status_request).unwrap_err();
        assert!(error.contains("Worker queue is full (not_started)"));
        assert!(rx.try_recv().is_ok());
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn caller_timeout_prevents_queued_request_from_starting_later() {
        let (ctx, rx) = context();
        let error = worker_call(&ctx, Duration::from_millis(100), status_request).unwrap_err();
        assert!(error.contains("not_started"));
        let queued = rx.try_recv().unwrap();
        assert!(queued.budget.begin().unwrap_err().contains("not_started"));
    }

    #[test]
    fn queue_rejection_cleans_the_generated_script() {
        let (ctx, _rx) = context();
        fill_queue(&ctx);
        let (path, script) = temporary_script();
        let error = worker_call(&ctx, Duration::from_secs(60), |response_tx| {
            script_request(&path, script, response_tx)
        })
        .unwrap_err();
        assert!(error.contains("not_started"));
        assert!(!path.exists());
    }

    #[test]
    fn caller_timeout_does_not_delete_a_worker_owned_script() {
        let (mut ctx, rx) = context();
        let (path, script) = temporary_script();
        ctx.request_started = Instant::now();
        let error = worker_call(&ctx, Duration::from_millis(100), |response_tx| {
            script_request(&path, script, response_tx)
        })
        .unwrap_err();
        assert!(error.contains("not_started"));
        let queued = rx.try_recv().unwrap();
        assert!(path.is_file());
        drop(queued);
        assert!(!path.exists());
    }

    #[test]
    fn expired_before_enqueue_cleans_the_generated_script() {
        let (mut ctx, rx) = context();
        ctx.request_started = Instant::now() - Duration::from_secs(1);
        let (path, script) = temporary_script();
        let error = worker_call(&ctx, Duration::from_millis(1), |response_tx| {
            script_request(&path, script, response_tx)
        })
        .unwrap_err();
        assert!(error.contains("not_started"));
        assert!(!path.exists());
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }
}
