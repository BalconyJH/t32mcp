// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_CONFIG_BYTES: usize = 16 * 1024;
const MAX_METADATA_BYTES: usize = 4096;
const PER_FILENAME: &str = "PER.FILENAME()";
const EVAL_TYPE_STRING: u32 = 0x0040;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PerSupport {
    #[default]
    Auto,
    Supported,
    Unsupported,
}

impl PerSupport {
    pub fn require_per(self) -> Result<(), String> {
        match self {
            Self::Unsupported => Err(
                "PER operations are disabled by user configuration (per_support=unsupported)"
                    .into(),
            ),
            Self::Auto | Self::Supported => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    #[serde(default)]
    pub per_support: PerSupport,
}

impl TargetConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            return Err("Target configuration must be an explicit .json file".into());
        }
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("Cannot inspect target configuration: {error}"))?;
        if !metadata.is_file() {
            return Err(
                "Target configuration must be a regular file, not a link or directory".into(),
            );
        }
        let file = File::open(path)
            .map_err(|error| format!("Cannot open target configuration: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("Cannot inspect open target configuration: {error}"))?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES as u64 {
            return Err(format!(
                "Target configuration must be a regular file of at most {MAX_CONFIG_BYTES} bytes"
            ));
        }
        let mut contents = Vec::new();
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut contents)
            .map_err(|error| format!("Cannot read target configuration: {error}"))?;
        if contents.len() > MAX_CONFIG_BYTES {
            return Err(format!(
                "Target configuration exceeds the {MAX_CONFIG_BYTES}-byte limit"
            ));
        }
        if contents.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            return Err("Target configuration must be a JSON object".into());
        }
        serde_json::from_slice(&contents)
            .map_err(|error| format!("Invalid target configuration JSON: {error}"))
    }
}

pub fn per_report_with(
    policy: PerSupport,
    connected: bool,
    evaluate: impl FnOnce(&str) -> Result<crate::Evaluation, String>,
) -> Value {
    let (status, source, evidence) = match policy {
        PerSupport::Auto => (
            "unknown",
            "auto",
            "Only active-definition metadata is queried; PER/MMIO read compatibility remains unproven.",
        ),
        PerSupport::Supported => (
            "user_declared_supported",
            "user_config",
            "Support is user-declared, not observed success; metadata does not prove PER/MMIO read compatibility or bypass safety checks.",
        ),
        PerSupport::Unsupported => (
            "user_declared_unsupported",
            "user_config",
            "User configuration disables PER operations; no metadata or peripheral read was attempted.",
        ),
    };
    let mut definition = if policy == PerSupport::Unsupported {
        json!({"status": "not_attempted", "reason": "Disabled by user configuration"})
    } else if !connected {
        json!({"status": "not_attempted", "reason": "No debugger connection"})
    } else {
        let attempted_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
            .ok();
        let mut report = definition_report(evaluate(PER_FILENAME));
        report["attempted_unix_ms"] = json!(attempted_unix_ms);
        report
    };
    definition["expression"] = json!(PER_FILENAME);
    json!({"status": status, "source": source, "evidence": evidence, "active_definition": definition})
}

fn definition_report(result: Result<crate::Evaluation, String>) -> Value {
    match result {
        Ok(result) if result.value.len() > MAX_METADATA_BYTES => json!({
            "status": "unknown", "result_type": result.result_type,
            "value_bytes": result.value.len(),
            "reason": "Native metadata exceeds the 4096-byte limit; value was not retained or interpreted",
        }),
        Ok(result) => {
            let (status, reason) = if result.result_type != EVAL_TYPE_STRING {
                ("unknown", "Native metadata result is not a string")
            } else if result.value.is_empty() {
                (
                    "absent",
                    "No active definition filename reported; PER support remains unproven",
                )
            } else {
                (
                    "present",
                    "Only a definition filename was observed; its contents and PER/MMIO compatibility were not checked",
                )
            };
            json!({"status": status, "reason": reason, "value": result.value, "result_type": result.result_type})
        }
        Err(error) => {
            let mut end = error.len().min(MAX_METADATA_BYTES);
            while !error.is_char_boundary(end) {
                end -= 1;
            }
            json!({"status": "unknown", "error": &error[..end], "error_truncated": end < error.len()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn load(contents: &[u8]) -> Result<TargetConfig, String> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("target.json");
        std::fs::write(&path, contents).unwrap();
        TargetConfig::load(&path)
    }

    fn evaluation(value: &str, result_type: u32) -> Result<crate::Evaluation, String> {
        Ok(crate::Evaluation {
            value: value.into(),
            result_type,
        })
    }

    #[test]
    fn configuration_defaults_to_auto_without_target_assumptions() {
        assert_eq!(TargetConfig::default().per_support, PerSupport::Auto);
        assert_eq!(load(b"{}").unwrap().per_support, PerSupport::Auto);
        for (name, policy) in [
            ("auto", PerSupport::Auto),
            ("supported", PerSupport::Supported),
            ("unsupported", PerSupport::Unsupported),
        ] {
            let text = format!("{{\"per_support\":\"{name}\"}}");
            assert_eq!(load(text.as_bytes()).unwrap().per_support, policy);
            assert_eq!(serde_json::to_value(policy).unwrap(), name);
        }
    }

    #[test]
    fn configuration_rejects_invalid_or_ambiguous_input() {
        for input in [
            "",
            " ",
            "null",
            "[]",
            "{} {}",
            "{\"cpu\":\"test\"}",
            "{\"per_support\":\"yes\"}",
            "{\"per_support\":true}",
            "{\"per_support\":null}",
            "{\"per_support\":\"auto\",\"per_support\":\"supported\"}",
        ] {
            assert!(load(input.as_bytes()).is_err(), "accepted {input:?}");
        }
        assert!(load(&[0xff]).is_err());
    }

    #[test]
    fn configuration_enforces_exact_byte_limit() {
        let mut contents = b"{}".to_vec();
        contents.resize(MAX_CONFIG_BYTES, b' ');
        assert!(load(&contents).is_ok());
        contents.push(b' ');
        assert!(load(&contents).unwrap_err().contains("16384"));
    }

    #[test]
    fn configuration_requires_existing_regular_json_file() {
        let directory = tempfile::tempdir().unwrap();
        assert!(TargetConfig::load(&directory.path().join("missing.json")).is_err());
        let folder = directory.path().join("folder.json");
        std::fs::create_dir(&folder).unwrap();
        assert!(TargetConfig::load(&folder).is_err());
        let wrong_extension = directory.path().join("target.txt");
        std::fs::write(&wrong_extension, b"{}").unwrap();
        assert!(TargetConfig::load(&wrong_extension).is_err());
    }

    #[test]
    fn unsupported_never_queries_metadata() {
        for connected in [false, true] {
            let report = per_report_with(PerSupport::Unsupported, connected, |_| {
                panic!("unexpected query")
            });
            assert_eq!(report["status"], "user_declared_unsupported");
            assert_eq!(report["source"], "user_config");
            assert_eq!(report["active_definition"]["status"], "not_attempted");
            assert!(
                report["active_definition"]
                    .get("attempted_unix_ms")
                    .is_none()
            );
        }
        assert!(
            PerSupport::Unsupported
                .require_per()
                .unwrap_err()
                .contains("disabled")
        );
    }

    #[test]
    fn disconnected_never_queries_metadata() {
        for policy in [PerSupport::Auto, PerSupport::Supported] {
            let report = per_report_with(policy, false, |_| panic!("unexpected query"));
            assert_eq!(report["active_definition"]["status"], "not_attempted");
            assert_eq!(
                report["active_definition"]["reason"],
                "No debugger connection"
            );
        }
    }

    #[test]
    fn metadata_does_not_prove_support_or_interpret_filename() {
        for (value, status) in [("", "absent"), (r"~~/untrusted;DO test.per", "present")] {
            let report = per_report_with(PerSupport::Auto, true, |expression| {
                assert_eq!(expression, "PER.FILENAME()");
                evaluation(value, EVAL_TYPE_STRING)
            });
            assert_eq!(report["status"], "unknown");
            assert_eq!(report["source"], "auto");
            assert_eq!(report["active_definition"]["status"], status);
            assert_eq!(report["active_definition"]["value"], value);
            assert_eq!(report["active_definition"]["result_type"], 0x0040);
            assert!(report["active_definition"]["attempted_unix_ms"].is_u64());
        }
    }

    #[test]
    fn unexpected_type_remains_unknown_and_preserves_raw_result() {
        for result_type in [0, 4, 8, 0x0041] {
            let report = per_report_with(PerSupport::Auto, true, |_| {
                evaluation("test.per", result_type)
            });
            assert_eq!(report["active_definition"]["status"], "unknown");
            assert_eq!(report["active_definition"]["value"], "test.per");
            assert_eq!(report["active_definition"]["result_type"], result_type);
        }
    }

    #[test]
    fn metadata_errors_are_preserved_but_not_inferred_as_unsupported() {
        let report = per_report_with(PerSupport::Auto, true, |_| {
            Err("T32_ExecuteFunction failed: 4290".into())
        });
        assert_eq!(report["status"], "unknown");
        assert_eq!(report["active_definition"]["status"], "unknown");
        assert_eq!(
            report["active_definition"]["error"],
            "T32_ExecuteFunction failed: 4290"
        );
        assert_eq!(report["active_definition"]["error_truncated"], false);
    }

    #[test]
    fn oversized_metadata_is_not_truncated_into_success() {
        let value = "x".repeat(MAX_METADATA_BYTES + 1);
        let report = per_report_with(PerSupport::Auto, true, |_| {
            evaluation(&value, EVAL_TYPE_STRING)
        });
        assert_eq!(report["active_definition"]["status"], "unknown");
        assert!(report["active_definition"].get("value").is_none());
        let value = "x".repeat(MAX_METADATA_BYTES);
        let report = per_report_with(PerSupport::Auto, true, |_| {
            evaluation(&value, EVAL_TYPE_STRING)
        });
        assert_eq!(report["active_definition"]["value"], value);
    }

    #[test]
    fn oversized_errors_are_explicitly_bounded_on_utf8_boundary() {
        let report = per_report_with(PerSupport::Auto, true, |_| {
            Err("\u{1f4a5}".repeat(MAX_METADATA_BYTES))
        });
        let error = report["active_definition"]["error"].as_str().unwrap();
        assert!(error.len() <= MAX_METADATA_BYTES);
        assert_eq!(report["active_definition"]["error_truncated"], true);
        assert_eq!(report["active_definition"]["status"], "unknown");
    }

    #[test]
    fn supported_declaration_survives_conflicting_metadata_without_proof() {
        for result in [
            evaluation("", EVAL_TYPE_STRING),
            Err("metadata unsupported".into()),
        ] {
            let report = per_report_with(PerSupport::Supported, true, |_| result);
            assert_eq!(report["status"], "user_declared_supported");
            assert_eq!(report["source"], "user_config");
            assert!(
                report["evidence"]
                    .as_str()
                    .unwrap()
                    .contains("not observed success")
            );
        }
    }

    #[test]
    fn every_connected_report_queries_fresh_metadata() {
        let calls = Cell::new(0);
        for value in ["first.per", "", "second.per"] {
            let report = per_report_with(PerSupport::Auto, true, |_| {
                calls.set(calls.get() + 1);
                evaluation(value, EVAL_TYPE_STRING)
            });
            assert_eq!(report["active_definition"]["value"], value);
        }
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn supported_policy_is_only_a_declaration_gate() {
        assert!(PerSupport::Auto.require_per().is_ok());
        assert!(PerSupport::Supported.require_per().is_ok());
        let report = per_report_with(PerSupport::Supported, false, |_| {
            panic!("must remain disconnected")
        });
        assert_eq!(report["active_definition"]["status"], "not_attempted");
        assert!(report["evidence"].as_str().unwrap().contains("not prove"));
        assert!(PerSupport::Unsupported.require_per().is_err());
    }
}
