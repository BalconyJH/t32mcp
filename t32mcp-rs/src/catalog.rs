// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use std::{fs, path::Path};

const MAX_DIRECTORY_ENTRIES: usize = 1024;
const MAX_SKILLS: usize = 64;
const MAX_SCRIPTS: usize = 256;

/// Discover skill metadata and script names without reading script contents.
pub fn list_skills(base: &Path) -> Result<Value, String> {
    let base = base
        .canonicalize()
        .map_err(|e| format!("Cannot open skills directory {}: {e}. Set --skills to a directory containing skills with SKILL.md and optional scripts/ directories.", base.display()))?;
    let entries = fs::read_dir(&base).map_err(|e| e.to_string())?;
    let mut skills = Vec::new();
    let mut truncated = false;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|e| e.to_string())?;
        if let Some((skill, scripts_truncated)) = discover_skill(&base, entry)? {
            skills.push(skill);
            truncated |= scripts_truncated;
        }
        if skills.len() >= MAX_SKILLS {
            truncated = true;
            break;
        }
    }
    skills.sort_by(|a, b| a["skill_name"].as_str().cmp(&b["skill_name"].as_str()));
    Ok(json!({
        "skills_base": base.to_string_lossy(),
        "skills": skills,
        "truncated": truncated,
    }))
}

fn discover_skill(base: &Path, entry: fs::DirEntry) -> Result<Option<(Value, bool)>, String> {
    let filename = entry.file_name();
    let filename = filename.to_string_lossy();
    let legacy_name = filename.strip_prefix("skill-");
    if filename.is_empty()
        || filename.len() > 128
        || legacy_name == Some("")
        || !filename
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Ok(None);
    }
    let Ok(skill_path) = entry.path().canonicalize() else {
        return Ok(None);
    };
    if !skill_path.is_dir() || !skill_path.starts_with(base) {
        return Ok(None);
    }
    let instructions_path = skill_path
        .join("SKILL.md")
        .canonicalize()
        .ok()
        .filter(|path| path.is_file() && path.starts_with(&skill_path));
    // Legacy installations may have only skill-*/scripts. New skill names
    // come from their directory and require a contained SKILL.md.
    if legacy_name.is_none() && instructions_path.is_none() {
        return Ok(None);
    }
    let scripts_path = skill_path
        .join("scripts")
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir() && path.starts_with(&skill_path));
    let (scripts, truncated) = if let Some(scripts_path) = scripts_path {
        let entries = fs::read_dir(&scripts_path).map_err(|e| e.to_string())?;
        discover_scripts(&scripts_path, entries)?
    } else if instructions_path.is_some() {
        (Vec::new(), false)
    } else {
        return Ok(None);
    };
    Ok(Some((
        json!({
            // The strict executor uses the directory name without adding a prefix.
            "skill_name": filename,
            "legacy_name": legacy_name,
            "skill_directory": skill_path.to_string_lossy(),
            "scripts": scripts,
            "instructions_path": instructions_path.map(|path| path.to_string_lossy().into_owned()),
        }),
        truncated,
    )))
}

fn discover_scripts(base: &Path, entries: fs::ReadDir) -> Result<(Vec<String>, bool), String> {
    let mut scripts = Vec::new();
    let mut truncated = false;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("cmm") {
            continue;
        }
        let Ok(resolved) = path.canonicalize() else {
            continue;
        };
        if resolved.is_file() && resolved.starts_with(base) {
            scripts.push(entry.file_name().to_string_lossy().into_owned());
        }
        if scripts.len() >= MAX_SCRIPTS {
            truncated = true;
            break;
        }
    }
    scripts.sort();
    Ok((scripts, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_names_without_interpreting_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("skill-fixture/scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("read.cmm"), [0xff, 0x00]).unwrap();
        fs::write(scripts.join("ignore.txt"), []).unwrap();
        let result = list_skills(dir.path()).unwrap();
        assert_eq!(result["skills"][0]["skill_name"], "skill-fixture");
        assert_eq!(result["skills"][0]["legacy_name"], "fixture");
        assert_eq!(result["skills"][0]["scripts"], json!(["read.cmm"]));
        assert_eq!(result["truncated"], false);
    }

    #[test]
    fn missing_directory_has_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            list_skills(&dir.path().join("missing"))
                .unwrap_err()
                .contains("--skills")
        );
    }

    #[test]
    fn discovers_upstream_skill_names_and_documentation_only_skills() {
        let dir = tempfile::tempdir().unwrap();
        let debug_skill = dir.path().join("lauterbach-trace32-debug-basic");
        let creator_skill = dir.path().join("lauterbach-trace32-skill-creator");
        fs::create_dir_all(debug_skill.join("scripts")).unwrap();
        fs::create_dir_all(&creator_skill).unwrap();
        for skill in [&debug_skill, &creator_skill] {
            fs::write(skill.join("SKILL.md"), "---\nname: fixture\n---\n").unwrap();
        }
        // Neither executable content nor valid text is needed for discovery.
        fs::write(debug_skill.join("scripts/get_state.cmm"), [0xff, 0x00]).unwrap();

        let result = list_skills(dir.path()).unwrap();
        let skills = result["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["skill_name"], "lauterbach-trace32-debug-basic");
        assert_eq!(skills[0]["scripts"], json!(["get_state.cmm"]));
        assert_eq!(
            skills[0]["skill_directory"],
            debug_skill
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(skills[1]["skill_name"], "lauterbach-trace32-skill-creator");
        assert_eq!(skills[1]["scripts"], json!([]));
        assert!(skills[1]["instructions_path"].as_str().is_some());
    }

    #[test]
    fn sorts_the_catalog_and_ignores_invalid_skills_and_nonfiles() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "skill-zeta",
            "skill-alpha",
            "skill-",
            "skill-bad name",
            "other",
        ] {
            let skill = dir.path().join(name);
            fs::create_dir_all(skill.join("scripts/not-a-file.cmm")).unwrap();
            fs::write(skill.join("scripts/z.cmm"), []).unwrap();
            fs::write(skill.join("scripts/a.cmm"), []).unwrap();
        }
        let instructions = dir.path().join("skill-alpha/SKILL.md");
        fs::write(&instructions, "Documented arguments").unwrap();
        let result = list_skills(dir.path()).unwrap();
        let skills = result["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["skill_name"], "skill-alpha");
        assert_eq!(skills[1]["skill_name"], "skill-zeta");
        assert_eq!(skills[0]["scripts"], json!(["a.cmm", "z.cmm"]));
        assert_eq!(
            skills[0]["instructions_path"],
            instructions
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert!(skills[1]["instructions_path"].is_null());
        assert_eq!(result["truncated"], false);
    }

    #[test]
    fn legacy_prefix_does_not_alias_another_skill() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["foo", "skill-foo"] {
            let skill = dir.path().join(name);
            fs::create_dir_all(skill.join("scripts")).unwrap();
            fs::write(skill.join("SKILL.md"), name).unwrap();
            fs::write(skill.join("scripts/read.cmm"), name).unwrap();
        }
        let result = list_skills(dir.path()).unwrap();
        let skills = result["skills"].as_array().unwrap();
        assert_eq!(skills[0]["skill_name"], "foo");
        assert_eq!(skills[1]["skill_name"], "skill-foo");
        assert!(skills[0]["legacy_name"].is_null());
        assert_eq!(skills[1]["legacy_name"], "foo");
        for skill in skills {
            let name = skill["skill_name"].as_str().unwrap();
            let script = dir.path().join(name).join("scripts/read.cmm");
            assert_eq!(fs::read_to_string(script).unwrap(), name);
        }
    }

    #[test]
    fn bounds_the_number_of_scripts_and_reports_partial_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join("skill-fixture/scripts");
        fs::create_dir_all(&scripts).unwrap();
        for index in 0..=MAX_SCRIPTS {
            fs::write(scripts.join(format!("{index}.cmm")), []).unwrap();
        }
        let result = list_skills(dir.path()).unwrap();
        assert_eq!(
            result["skills"][0]["scripts"].as_array().unwrap().len(),
            MAX_SCRIPTS
        );
        assert_eq!(result["truncated"], true);
    }

    #[cfg(unix)]
    #[test]
    fn excludes_symlinks_that_escape_skill_and_script_boundaries() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("scripts")).unwrap();
        fs::write(outside.path().join("SKILL.md"), "outside").unwrap();
        fs::write(outside.path().join("scripts/outside.cmm"), []).unwrap();
        symlink(outside.path(), dir.path().join("skill-escaped")).unwrap();

        let skill = dir.path().join("lauterbach-trace32-debug-basic");
        fs::create_dir_all(skill.join("scripts")).unwrap();
        fs::write(skill.join("SKILL.md"), "inside").unwrap();
        fs::write(skill.join("scripts/inside.cmm"), []).unwrap();
        symlink(
            outside.path().join("scripts/outside.cmm"),
            skill.join("scripts/escaped.cmm"),
        )
        .unwrap();

        let escaped_doc = dir.path().join("lauterbach-trace32-escaped-doc");
        fs::create_dir_all(&escaped_doc).unwrap();
        symlink(
            outside.path().join("SKILL.md"),
            escaped_doc.join("SKILL.md"),
        )
        .unwrap();
        let escaped_scripts = dir.path().join("lauterbach-trace32-escaped-scripts");
        fs::create_dir_all(&escaped_scripts).unwrap();
        fs::write(escaped_scripts.join("SKILL.md"), "inside").unwrap();
        symlink(
            outside.path().join("scripts"),
            escaped_scripts.join("scripts"),
        )
        .unwrap();

        let result = list_skills(dir.path()).unwrap();
        let skills = result["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["skill_name"], "lauterbach-trace32-debug-basic");
        assert_eq!(skills[0]["scripts"], json!(["inside.cmm"]));
        assert_eq!(
            skills[1]["skill_name"],
            "lauterbach-trace32-escaped-scripts"
        );
        assert_eq!(skills[1]["scripts"], json!([]));
    }
}
