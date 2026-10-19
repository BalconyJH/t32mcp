// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use std::{fs, path::Path};

const MAX_DIRECTORY_ENTRIES: usize = 1024;
const MAX_SKILLS: usize = 64;
const MAX_SCRIPTS: usize = 256;

/// Discover installed scripts without interpreting or reading their contents.
pub fn list_skills(base: &Path) -> Result<Value, String> {
    let base = base
        .canonicalize()
        .map_err(|e| format!("Cannot open skills directory {}: {e}. Set --skills to a directory containing skill-*/scripts.", base.display()))?;
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
    let Some(name) = filename.strip_prefix("skill-").filter(|name| {
        !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    }) else {
        return Ok(None);
    };
    let Ok(skill_path) = entry.path().canonicalize() else {
        return Ok(None);
    };
    if !skill_path.is_dir() || !skill_path.starts_with(base) {
        return Ok(None);
    }
    let Ok(scripts_path) = skill_path.join("scripts").canonicalize() else {
        return Ok(None);
    };
    if !scripts_path.starts_with(&skill_path) {
        return Ok(None);
    }
    let Ok(entries) = fs::read_dir(&scripts_path) else {
        return Ok(None);
    };
    let (scripts, truncated) = discover_scripts(&scripts_path, entries)?;
    let instructions_path = skill_path.join("SKILL.md");
    Ok(Some((
        json!({
            "skill_name": name,
            "scripts": scripts,
            "instructions_path": instructions_path.is_file().then(|| instructions_path.to_string_lossy().into_owned()),
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
        assert_eq!(result["skills"][0]["skill_name"], "fixture");
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
        assert_eq!(skills[0]["skill_name"], "alpha");
        assert_eq!(skills[1]["skill_name"], "zeta");
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
}
