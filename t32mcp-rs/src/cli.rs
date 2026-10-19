// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use rand::RngExt;
use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct T32Options {
    pub unlock_all_tools: bool,
    pub read_only: bool,
    pub native_tools: bool,
    pub skills_base: String,
    pub artifacts_base: PathBuf,
    pub port: u16,
    pub pipe_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpTransport {
    Stdio,
    Http { port: u16 },
}

#[derive(Debug)]
pub(crate) enum CliAction {
    Run(T32Options, McpTransport),
    Help,
    Version,
}

fn parse_port(value: Option<String>, option: &str) -> Result<u16, String> {
    let value = value.ok_or_else(|| format!("{option} requires a port"))?;
    let port = value
        .parse::<u16>()
        .map_err(|_| format!("Invalid {option} port {value:?}: expected 1..65535"))?;
    if port == 0 {
        return Err(format!("Invalid {option} port: expected 1..65535"));
    }
    Ok(port)
}

pub(crate) fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut options = T32Options {
        unlock_all_tools: false,
        read_only: false,
        native_tools: false,
        skills_base: "".to_string(),
        artifacts_base: PathBuf::new(),
        port: 20000,
        pipe_name: format!("t32chat.{}", rand::rng().random::<u64>()),
    };
    let mut args = args.into_iter();
    let mut transport = McpTransport::Stdio;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "--version" => return Ok(CliAction::Version),
            "--t32chat" => options.unlock_all_tools = true,
            "--native-tools" => options.native_tools = true,
            "--read-only" => options.read_only = true,
            "--skills" => {
                let base = args
                    .next()
                    .ok_or_else(|| "--skills requires a directory".to_string())?;
                let base = base.trim_matches('"');
                if base.trim().is_empty() || base.chars().any(char::is_control) {
                    return Err("--skills requires a nonempty, valid directory".to_string());
                }
                options.skills_base = base.to_string();
            }
            "--artifacts" => {
                let directory = args
                    .next()
                    .ok_or_else(|| "--artifacts requires an absolute directory".to_string())?;
                let directory = directory.trim_matches('"');
                if directory.trim().is_empty() || directory.chars().any(char::is_control) {
                    return Err("--artifacts requires a nonempty, valid directory".into());
                }
                let directory = PathBuf::from(directory);
                if !directory.is_absolute() {
                    return Err("--artifacts requires an absolute directory".into());
                }
                options.artifacts_base = directory;
            }
            "--port" => options.port = parse_port(args.next(), "--port")?,
            "--http" => {
                transport = McpTransport::Http {
                    port: parse_port(args.next(), "--http")?,
                };
            }
            _ => return Err(format!("Unknown argument: {arg:?}. Use --help for usage.")),
        }
    }
    Ok(CliAction::Run(options, transport))
}

pub(crate) fn default_skills_base() -> Result<String, String> {
    let path: PathBuf = if cfg!(target_os = "windows") {
        let appdata = env::var("APPDATA")
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "Skills directory cannot be found (APPDATA not set); use --skills".to_string()
            })?;
        [appdata.as_str(), "TRACE32", "skills"].iter().collect()
    } else {
        let config_home = env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                env::var("HOME")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(|home| format!("{home}/.config"))
            })
            .ok_or_else(|| {
                "Skills directory cannot be found (HOME not set); use --skills".to_string()
            })?;
        [config_home.as_str(), "TRACE32", "skills"].iter().collect()
    };
    Ok(path.to_string_lossy().into_owned())
}

pub(crate) fn default_artifacts_base() -> PathBuf {
    if cfg!(target_os = "windows") {
        env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|root| root.join("t32mcp").join("evidence"))
            .unwrap_or_default()
    } else {
        env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".local").join("state"))
            })
            .map(|root| root.join("t32mcp").join("evidence"))
            .unwrap_or_default()
    }
}

pub(crate) fn print_version() {
    let v = env!("CARGO_PKG_VERSION").to_owned();
    let v = format!("t32mcp v{v}");

    #[cfg(debug_assertions)]
    let v = format!("{v}.debug");

    println!("{v}");
}

pub(crate) fn print_help() {
    print_version();
    println!(
        "Options:
  --http PORT      Serve MCP over loopback HTTP (no authentication)
  --port PORT      TRACE32 Remote API TCP port (default: 20000)
  --skills DIR     Skills base directory (default: %APPDATA%/TRACE32/skills)
  --artifacts DIR  Local evidence directory (default: %LOCALAPPDATA%/t32mcp/evidence)
  --native-tools   Enable optional native inspection and evidence tools
  --t32chat        Unlocks execute_practice tool independently of --native-tools
  --read-only      Hide script execution and abort tools (overrides --t32chat)
  --version       Print version number
  -h, --help       Print this help message

Default MCP surface contains three generic PRACTICE skill tools.
Connection is local and lazy. --read-only disables execution, not MMIO side effects.
Polling deadlines and pipe acceptance do not interrupt blocked native API calls.
Evidence is written only when save_artifact=true. Old artifacts are never overwritten."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Result<CliAction, String> {
        parse_args(args.iter().map(|arg| (*arg).to_string()))
    }

    #[test]
    fn defaults_preserve_the_generic_practice_interface() {
        let CliAction::Run(options, transport) = cli(&[]).unwrap() else {
            panic!("Expected server options");
        };
        assert!(!options.native_tools);
        assert!(!options.unlock_all_tools);
        assert!(!options.read_only);
        assert_eq!(options.port, 20000);
        assert!(options.skills_base.is_empty());
        assert!(options.artifacts_base.as_os_str().is_empty());
        assert!(options.pipe_name.starts_with("t32chat."));
        assert_eq!(transport, McpTransport::Stdio);
    }

    #[test]
    fn cli_rejects_invalid_arguments_without_panicking() {
        for args in [
            vec!["--port"],
            vec!["--port", "0"],
            vec!["--port", "65536"],
            vec!["--port", "invalid"],
            vec!["--port", "-1"],
            vec!["--http", "0"],
            vec!["--http", "-1"],
            vec!["--http", "65536"],
            vec!["--http", "invalid"],
            vec!["--http"],
            vec!["--skills"],
            vec!["--skills", ""],
            vec!["--skills", "  "],
            vec!["--skills", "a\0b"],
            vec!["--artifacts"],
            vec!["--artifacts", ""],
            vec!["--artifacts", "relative"],
            vec!["--artifacts", "a\0b"],
            vec!["--unknown"],
            vec!["--native-tools=true"],
        ] {
            assert!(cli(&args).is_err(), "accepted invalid arguments: {args:?}");
        }
    }

    #[test]
    fn cli_supports_help_and_version_without_resolving_paths() {
        assert!(matches!(cli(&["-h"]).unwrap(), CliAction::Help));
        assert!(matches!(cli(&["--help"]).unwrap(), CliAction::Help));
        assert!(matches!(cli(&["--version"]).unwrap(), CliAction::Version));
    }

    #[test]
    fn cli_preserves_transport_and_directory_options() {
        let directory = if cfg!(windows) {
            r"C:\local evidence"
        } else {
            "/local evidence"
        };
        let CliAction::Run(options, transport) = cli(&[
            "--port",
            "65535",
            "--http",
            "1",
            "--skills",
            "\"skills with spaces\"",
            "--artifacts",
            directory,
        ])
        .unwrap() else {
            panic!("Expected server options");
        };
        assert_eq!(options.port, 65535);
        assert_eq!(options.skills_base, "skills with spaces");
        assert_eq!(options.artifacts_base, PathBuf::from(directory));
        assert_eq!(transport, McpTransport::Http { port: 1 });
    }

    #[test]
    fn cli_feature_flags_are_independent_and_order_insensitive() {
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
            let mut args = Vec::new();
            if native_tools {
                args.push("--native-tools");
            }
            if read_only {
                args.push("--read-only");
            }
            if unlock_all_tools {
                args.push("--t32chat");
            }
            for _ in 0..2 {
                let CliAction::Run(options, _) = cli(&args).unwrap() else {
                    panic!("Expected server options");
                };
                assert_eq!(options.native_tools, native_tools);
                assert_eq!(options.read_only, read_only);
                assert_eq!(options.unlock_all_tools, unlock_all_tools);
                args.reverse();
            }
        }
    }
}
