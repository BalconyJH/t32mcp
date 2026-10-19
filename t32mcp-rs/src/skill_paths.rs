// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, DefaultHasher};
use std::path::{Component, Path, PathBuf};

type ScriptArgs = HashMap<String, String, BuildHasherDefault<DefaultHasher>>;

fn single_component(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains(['/', '\\', ':'])
        || value.chars().any(char::is_control)
        || value.contains(['"', '&'])
        || value.trim() != value
        || value.ends_with('.')
        || !matches!(
            Path::new(value).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        )
    {
        return Err(format!("{label} must be a single safe path component"));
    }
    Ok(())
}

fn canonical_child(parent: &Path, child: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = child
        .canonicalize()
        .map_err(|e| format!("Cannot resolve {label} {}: {e}", child.display()))?;
    if canonical == parent || !canonical.starts_with(parent) {
        return Err(format!("{label} escapes its allowed directory"));
    }
    Ok(canonical)
}

pub(crate) fn resolve_script(
    base: &str,
    skill_name: &str,
    script_name: &str,
) -> Result<PathBuf, String> {
    single_component(skill_name, "skill_name")?;
    single_component(script_name, "script_name")?;
    if !script_name.ends_with(".cmm") {
        return Err("Not a valid PRACTICE (*.cmm) script".to_string());
    }

    let base = Path::new(base)
        .canonicalize()
        .map_err(|e| format!("Cannot resolve skills directory: {e}"))?;
    let skill = canonical_child(
        &base,
        &base.join(format!("skill-{skill_name}")),
        "Skill directory",
    )?;
    let scripts = canonical_child(&skill, &skill.join("scripts"), "Scripts directory")?;
    let script = canonical_child(&scripts, &scripts.join(script_name), "PRACTICE script")?;
    if !script.is_file() {
        return Err(format!("Not a file: {}", script.display()));
    }
    Ok(script)
}

pub(crate) fn quote_practice_literal(value: &str, label: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) || value.contains('"') {
        return Err(format!(
            "{label} cannot contain control characters or double quotes"
        ));
    }
    // Do not interpolate PRACTICE macros into a generated command line.
    if value
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b'&' && (pair[1].is_ascii_alphanumeric() || pair[1] == b'_'))
    {
        return Err(format!("{label} cannot contain a PRACTICE macro reference"));
    }
    Ok(format!("\"{value}\""))
}

pub(crate) fn do_command(path: &Path, args: Option<&ScriptArgs>) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| "PRACTICE script path must be valid Unicode".to_string())?;
    #[cfg(windows)]
    let normalized = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    };
    #[cfg(windows)]
    let path = normalized.as_str();
    let mut command = format!("DO {}", quote_practice_literal(path, "Script path")?);
    if let Some(args) = args {
        let mut args = args.iter().collect::<Vec<_>>();
        args.sort_unstable_by_key(|(key, _)| *key);
        for (key, value) in args {
            let mut chars = key.bytes();
            if !chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                || !chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return Err(format!("Invalid script argument key: {key:?}"));
            }
            command.push(' ');
            command.push_str(&quote_practice_literal(
                &format!("{key}={value}"),
                "Script argument",
            )?);
        }
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let scripts = temp.path().join("skill-test").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("read.cmm"), []).unwrap();
        temp
    }

    #[test]
    fn resolves_only_a_script_within_its_skills_directory() {
        let temp = fixture();
        let path = resolve_script(temp.path().to_str().unwrap(), "test", "read.cmm").unwrap();
        assert!(path.starts_with(temp.path().canonicalize().unwrap()));
        for skill in ["", ".", "..", "../test", r"..\test", r"C:\test", "test."] {
            assert!(resolve_script(temp.path().to_str().unwrap(), skill, "read.cmm").is_err());
        }
        for script in [
            "../read.cmm",
            r"..\read.cmm",
            "/read.cmm",
            "read.cmm:stream",
            "read.txt",
            "read\0.cmm",
        ] {
            assert!(resolve_script(temp.path().to_str().unwrap(), "test", script).is_err());
        }
    }

    #[test]
    fn quotes_arguments_deterministically_without_command_injection() {
        let mut args = ScriptArgs::default();
        args.insert("pattern".into(), "&&".into());
        args.insert("address".into(), "main + 0x10".into());
        assert_eq!(
            do_command(Path::new("script.cmm"), Some(&args)).unwrap(),
            "DO \"script.cmm\" \"address=main + 0x10\" \"pattern=&&\""
        );
        for key in ["", " bad", "key name", "a=b", "a\" END", "a\n", "1key"] {
            let mut args = ScriptArgs::default();
            args.insert(key.into(), "value".into());
            assert!(do_command(Path::new("script.cmm"), Some(&args)).is_err());
        }
        for value in ["a\0b", "a\nEND", "a\rEND", "a\tb", "\"; END", "&macro"] {
            let mut args = ScriptArgs::default();
            args.insert("key".into(), value.into());
            assert!(do_command(Path::new("script.cmm"), Some(&args)).is_err());
        }
        assert!(do_command(Path::new("script\"\nEND.cmm"), None).is_err());
    }

    #[test]
    fn canonical_containment_rejects_a_sibling_even_without_symlink_privileges() {
        let temp = fixture();
        let scripts = temp
            .path()
            .join("skill-test/scripts")
            .canonicalize()
            .unwrap();
        let sibling = temp.path().join("outside.cmm");
        std::fs::write(&sibling, []).unwrap();
        assert!(canonical_child(&scripts, &sibling, "PRACTICE script").is_err());
        assert!(canonical_child(&scripts, &scripts, "PRACTICE script").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_windows_canonical_paths_for_trace32() {
        assert_eq!(
            do_command(Path::new(r"\\?\C:\skills\read.cmm"), None).unwrap(),
            r#"DO "C:\skills\read.cmm""#
        );
        assert_eq!(
            do_command(Path::new(r"\\?\UNC\server\skills\read.cmm"), None).unwrap(),
            r#"DO "\\server\skills\read.cmm""#
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_script_symlink_escape() {
        let temp = fixture();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("outside.cmm");
        std::fs::write(&target, []).unwrap();
        std::os::unix::fs::symlink(target, temp.path().join("skill-test/scripts/link.cmm"))
            .unwrap();
        assert!(resolve_script(temp.path().to_str().unwrap(), "test", "link.cmm").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_script_symlink_escape_when_symlinks_are_available() {
        let temp = fixture();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("outside.cmm");
        std::fs::write(&target, []).unwrap();
        match std::os::windows::fs::symlink_file(
            target,
            temp.path().join("skill-test/scripts/link.cmm"),
        ) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => return,
            Err(error) => panic!("Cannot create test symlink: {error}"),
        }
        assert!(resolve_script(temp.path().to_str().unwrap(), "test", "link.cmm").is_err());
    }
}
