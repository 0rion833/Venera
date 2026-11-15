//! Skills: reusable instruction packs, in the `SKILL.md` format.
//!
//! A skill is a directory with a `SKILL.md` file. The front matter carries the name and a
//! one-line description; the body is the instruction text itself. Loading is deliberately
//! dumb: no network, no model, no execution — the text is shown or attached to context.
//!
//! Two locations are searched, project first:
//!
//! - `<project>/.venera/skills/<name>/SKILL.md`
//! - `<user config>/skills/<name>/SKILL.md` (for example `~/.config/venera/skills`)

use std::path::{Path, PathBuf};

/// Instruction pack shipped with the binary (`src/skill.md`).
const BUILTIN_SKILL: &str = include_str!("skill.md");

/// The built-in Venera CLI skill: always available, even in an empty project.
fn builtin() -> Skill {
    parse(Path::new("<built-in>/SKILL.md"), BUILTIN_SKILL)
}

/// One loaded skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Skill name, from the front matter or the directory name.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Where it was loaded from.
    pub path: PathBuf,
    /// Instruction text, without the front matter.
    pub body: String,
}

impl Skill {
    /// First lines of the body, for listings.
    pub fn preview(&self, lines: usize) -> String {
        self.body
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(lines)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Project skill directory.
pub fn project_dir(root: &Path) -> PathBuf {
    root.join(".venera").join("skills")
}

/// User skill directory, when a config directory is available.
pub fn user_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "venera").map(|dirs| dirs.config_dir().join("skills"))
}

/// Discovers project, user, executable-adjacent, and built-in skills.
///
/// A project skill wins on a name clash; the executable directory is read so a portable
/// copy can ship `SKILL.md` next to `venera.exe`; the built-in pack is the last resort.
pub fn discover(root: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    collect(&project_dir(root), &mut skills);

    if let Some(dir) = user_dir() {
        collect(&dir, &mut skills);
    }

    if let Some(dir) = exe_dir() {
        let loose = dir.join("SKILL.md");
        if let Ok(text) = std::fs::read_to_string(&loose) {
            let skill = parse(&loose, &text);
            if !skills.iter().any(|existing| existing.name == skill.name) {
                skills.push(skill);
            }
        }
        collect(&dir.join("skills"), &mut skills);
    }

    let built_in = builtin();
    if !skills.iter().any(|skill| skill.name == built_in.name) {
        skills.push(built_in);
    }

    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills
}

/// Directory of the running executable.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

fn collect(dir: &Path, skills: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path().join("SKILL.md");
        if !path.is_file() {
            continue;
        }

        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let skill = parse(&path, &text);

        if !skills.iter().any(|existing| existing.name == skill.name) {
            skills.push(skill);
        }
    }
}

/// Parses front matter and body.
///
/// An unreadable or missing front matter is not an error: the directory name becomes the
/// name and the first line becomes the description.
fn parse(path: &Path, text: &str) -> Skill {
    let fallback_name = path
        .parent()
        .and_then(|dir| dir.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "skill".to_owned());

    let mut name = fallback_name;
    let mut description = String::new();
    let mut body = text.trim().to_owned();

    if let Some(rest) = text.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        let front = &rest[..end];
        body = rest[end + 4..].trim().to_owned();

        for line in front.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().trim_matches('"').trim_matches('\'').to_owned();
            match key.trim() {
                "name" if !value.is_empty() => name = value,
                "description" if !value.is_empty() => description = value,
                _ => {}
            }
        }
    }

    if description.is_empty() {
        description = body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .unwrap_or("")
            .to_owned();
    }

    Skill {
        name,
        description,
        path: path.to_path_buf(),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, text: &str) {
        let dir = project_dir(root).join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("SKILL.md"), text).expect("write");
    }

    #[test]
    fn reads_front_matter_and_body() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_skill(
            dir.path(),
            "review",
            "---\nname: review\ndescription: Review a diff before committing\n---\n\nRead the diff first.\n",
        );

        let skills = discover(dir.path());
        let review = skills
            .iter()
            .find(|skill| skill.name == "review")
            .expect("project skill is loaded");

        assert_eq!(review.description, "Review a diff before committing");
        assert!(review.body.starts_with("Read the diff first."));
        assert!(!review.body.contains("name: review"));
    }

    #[test]
    fn falls_back_to_the_directory_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_skill(dir.path(), "notes", "Summarise the meeting.\n");

        let skills = discover(dir.path());

        assert_eq!(skills[0].name, "notes");
        assert_eq!(skills[0].description, "Summarise the meeting.");
    }

    #[test]
    fn an_empty_project_still_has_the_builtin_skill() {
        let dir = tempfile::tempdir().expect("temp dir");
        let skills = discover(dir.path());

        assert!(skills.iter().any(|skill| skill.name == "venera-cli"));
    }

    #[test]
    fn preview_skips_blank_lines() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_skill(dir.path(), "demo", "---\nname: demo\n---\n\none\n\ntwo\n");

        let skills = discover(dir.path());
        assert_eq!(skills[0].preview(2), "one\ntwo");
    }
}
