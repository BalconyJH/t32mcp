// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use tokio::task::JoinHandle;

pub(super) const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

// Provides access to the AREA output of TRACE32
#[derive(Debug)]
pub(super) struct T32Sink {
    pub(super) active: bool,
    received_data: String,
    pub(super) truncated: bool,
    pub(super) generation: u64,
    pub(super) reader: Option<JoinHandle<()>>,
}

impl T32Sink {
    pub(super) fn new() -> Self {
        Self {
            active: false,
            received_data: String::new(),
            truncated: false,
            generation: 0,
            reader: None,
        }
    }

    pub(super) fn stop_reader(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        self.active = false;
    }

    pub(super) fn reset(&mut self) {
        self.stop_reader();
        self.received_data.clear();
        self.truncated = false;
    }

    pub(super) fn activate(&mut self) -> u64 {
        self.reset();
        self.active = true;
        self.generation
    }

    pub(super) fn append(&mut self, message: &str) {
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(self.received_data.len());
        let mut length = message.len().min(remaining);
        while !message.is_char_boundary(length) {
            length -= 1;
        }
        self.received_data.push_str(&message[..length]);
        self.truncated |= length < message.len();
    }

    pub(super) fn response(&mut self) -> String {
        let status = if self.active {
            "<NOT FINISHED>"
        } else {
            "<FINISHED>"
        };
        let response = format!(
            "{status}\n<TRUNCATED>{}</TRUNCATED>\n<CONTENT>\n{}",
            self.truncated, self.received_data
        );
        if !self.active {
            self.stop_reader();
            self.received_data.clear();
            self.truncated = false;
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_bounded_and_preserves_status_markers() {
        let mut sink = T32Sink::new();
        sink.activate();
        sink.append(&"x".repeat(MAX_OUTPUT_BYTES - 1));
        sink.append("\u{e9}");
        sink.append("last bytes");
        assert!(sink.received_data.len() <= MAX_OUTPUT_BYTES);
        assert!(sink.truncated);
        let response = sink.response();
        assert!(response.starts_with("<NOT FINISHED>"));
        assert!(response.contains("<TRUNCATED>true</TRUNCATED>"));
        assert!(!sink.received_data.is_empty());
        sink.stop_reader();
        assert!(sink.response().starts_with("<FINISHED>"));
        assert!(sink.received_data.is_empty());
        assert!(!sink.truncated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sink_reset_aborts_reader_and_invalidates_its_generation() {
        let mut sink = T32Sink::new();
        let generation = sink.activate();
        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        sink.reader = Some(task);
        sink.append("old output");
        sink.reset();
        assert!(abort.is_finished() || sink.reader.is_none());
        assert_ne!(generation, sink.generation);
        assert!(!sink.active);
        assert!(sink.received_data.is_empty());
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }
}
