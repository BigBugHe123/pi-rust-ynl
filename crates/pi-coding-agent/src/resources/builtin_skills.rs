use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::digest::{Context, SHA256};

use super::ResourceDiagnostic;
use super::paths::absolute;
use super::skills::{LoadSkillsResult, load_skill_from_file};

type BundledFiles = &'static [(&'static str, &'static str)];

const BUILTIN_SKILLS: &[(&str, BundledFiles)] = &[
    (
        "pi-extension-authoring",
        &[
            ("SKILL.md", include_str!("../../assets/skills/pi-extension-authoring/SKILL.md")),
            ("native-protocol.md", include_str!("../../assets/skills/pi-extension-authoring/native-protocol.md")),
        ],
    ),
    ("pi-mcp-management", &[("SKILL.md", include_str!("../../assets/skills/pi-mcp-management/SKILL.md"))]),
    ("pi-skill-authoring", &[("SKILL.md", include_str!("../../assets/skills/pi-skill-authoring/SKILL.md"))]),
];

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(super) fn add_builtin_skills(agent_dir: &Path, result: &mut LoadSkillsResult) {
    let cache = absolute(agent_dir).join("cache").join("builtin-skills");
    for &(name, files) in BUILTIN_SKILLS {
        if result.skills.iter().any(|skill| skill.name == name) {
            continue;
        }
        let mut hash = Context::new(&SHA256);
        for &(filename, contents) in files {
            hash.update(filename.as_bytes());
            hash.update(&[0]);
            hash.update(contents.as_bytes());
            hash.update(&[0]);
        }
        let dir = cache.join(URL_SAFE_NO_PAD.encode(hash.finish().as_ref())).join(name);
        if let Err(error) = fs::create_dir_all(&dir)
            .and_then(|()| files.iter().try_for_each(|(filename, contents)| cache_file(&dir.join(filename), contents)))
        {
            result
                .diagnostics
                .push(ResourceDiagnostic::warning(format!("could not cache built-in skill: {error}"), &dir));
            continue;
        }
        let (skill, diagnostics) = load_skill_from_file(&dir.join("SKILL.md"), "builtin");
        result.skills.extend(skill);
        result.diagnostics.extend(diagnostics);
    }
}

fn cache_file(path: &Path, contents: &str) -> io::Result<()> {
    if fs::read(path).is_ok_and(|cached| cached == contents.as_bytes()) {
        return Ok(());
    }
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed)));
    let mut file = OpenOptions::new().write(true).create_new(true).open(&temp)?;
    let written = file.write_all(contents.as_bytes());
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
