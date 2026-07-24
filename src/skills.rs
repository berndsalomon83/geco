//! Skills on demand — the same disease geco cures for tools, cured
//! for agent skills. opencode (and Claude) load every skill's name and
//! description into the system context at startup: 36 skills measured at
//! ~9.2 KB ≈ 2,300 tokens per session, used or not. Behind the gateway the
//! cost is three small meta-tools (list / search / read), and the library
//! can grow without bound.
//!
//! A skill is a directory with a SKILL.md whose YAML frontmatter carries
//! `name` and `description`. `read_skill` returns the full body plus the
//! skill's file tree (absolute paths), so the agent can run bundled scripts
//! via its shell exactly as a natively-loaded skill would instruct.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Recursively copy a directory tree (used as a cross-filesystem fallback
/// for `rename` in `absorb_into`).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let p = entry.path();
        let target = dst.join(entry.file_name());
        if p.is_dir() {
            copy_dir_recursive(&p, &target)?;
        } else {
            std::fs::copy(&p, &target)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, Default)]
pub struct SkillIndex {
    pub skills: Vec<SkillMeta>,
    pub errors: Vec<String>,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: Option<String>,
}

fn parse_frontmatter(text: &str) -> Option<Frontmatter> {
    let rest = text.strip_prefix("---")?;
    // Try each candidate closing delimiter. A YAML literal block scalar
    // can contain lines starting with ---, so we can't just find the first
    // match — we try parsing at each candidate and return the first that
    // yields valid frontmatter.
    let mut search_from = 0;
    loop {
        let pos = rest[search_from..].find("\n---")?;
        let abs = search_from + pos;
        // Candidate: text before this \n--- is the YAML frontmatter.
        let candidate = &rest[..abs];
        // The --- must be at the start of a line (followed by EOF/whitespace).
        let after = &rest[abs + 4..];
        let looks_like_delimiter = after.is_empty()
            || after.starts_with('\n')
            || after.starts_with('\r')
            || after.starts_with(' ')
            || after.starts_with('\t');
        if !looks_like_delimiter {
            search_from = abs + 4;
            continue;
        }
        // Try parsing — if it works, this is the real closing delimiter.
        if let Ok(fm) = serde_norway::from_str::<Frontmatter>(candidate) {
            return Some(fm);
        }
        // Parse failed — this --- was inside a block scalar. Keep searching.
        search_from = abs + 4;
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        // On Windows, HOME is typically unset — fall back to USERPROFILE.
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        if let Some(home) = home {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Sweep skills out of `absorb_dirs` into `library` (the first configured
/// skills dir). Runs at startup and on refresh — the native auto-loaded dir
/// stays empty forever, so no skill ever costs always-on context, no matter
/// where a model or installer drops it. Collisions are skipped and reported;
/// non-skill entries are left alone.
pub fn absorb_into(library: &str, absorb_dirs: &[String]) -> (Vec<String>, Vec<String>) {
    let lib = expand_tilde(library);
    let mut moved = Vec::new();
    let mut errors = Vec::new();
    if std::fs::create_dir_all(&lib).is_err() {
        errors.push(format!("cannot create library {}", lib.display()));
        return (moved, errors);
    }
    for dir in absorb_dirs {
        let src_root = expand_tilde(dir);
        if src_root == lib {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&src_root) else {
            continue;
        };
        for entry in entries.flatten() {
            let src = entry.path();
            if !src.join("SKILL.md").is_file() {
                continue; // not a skill — leave it alone
            }
            let name = src.file_name().unwrap_or_default().to_owned();
            let dst = lib.join(&name);
            if dst.exists() {
                errors.push(format!(
                    "absorb skipped '{}': already in library (resolve manually)",
                    name.to_string_lossy()
                ));
                continue;
            }
            match std::fs::rename(&src, &dst) {
                Ok(()) => moved.push(name.to_string_lossy().to_string()),
                Err(e) => {
                    // rename fails with EXDEV across filesystems — fall back
                    // to copy + remove so absorb works on cross-mounted dirs.
                    if let Err(copy_err) = copy_dir_recursive(&src, &dst) {
                        errors.push(format!(
                            "absorb failed for '{}': rename: {e}; copy fallback: {copy_err}",
                            name.to_string_lossy()
                        ));
                        continue;
                    }
                    if let Err(rm_err) = std::fs::remove_dir_all(&src) {
                        errors.push(format!(
                            "absorb partially failed for '{}': copied but could not remove source: {rm_err}",
                            name.to_string_lossy()
                        ));
                        continue;
                    }
                    moved.push(name.to_string_lossy().to_string());
                }
            }
        }
    }
    (moved, errors)
}

impl SkillIndex {
    pub fn scan(dirs: &[String]) -> SkillIndex {
        let mut idx = SkillIndex::default();
        for dir in dirs {
            let root = expand_tilde(dir);
            let entries = match std::fs::read_dir(&root) {
                Ok(e) => e,
                Err(e) => {
                    idx.errors.push(format!("{}: {e}", root.display()));
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let skill_md = path.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&skill_md) else {
                    continue;
                };
                let fm = parse_frontmatter(&text);
                let name = fm.as_ref().and_then(|f| f.name.clone()).unwrap_or_else(|| {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                });
                let (description, version) = match fm {
                    Some(f) => (f.description, f.version),
                    None => (String::new(), None),
                };
                idx.skills.push(SkillMeta {
                    name,
                    description,
                    version,
                    dir: path,
                });
            }
        }
        idx.skills.sort_by(|a, b| a.name.cmp(&b.name));
        idx
    }

    /// Ranked by the same scorer as tool search, so the two surfaces behave
    /// alike. Skills have no server field.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&SkillMeta> {
        let docs: Vec<crate::search::Doc> = self
            .skills
            .iter()
            .map(|s| crate::search::Doc {
                name: &s.name,
                description: &s.description,
                server: None,
            })
            .collect();
        crate::search::rank(&docs, query, limit)
            .into_iter()
            .map(|(i, _)| &self.skills[i])
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.iter().find(|s| s.name == name)
    }
}

/// SKILL.md body + the skill's file tree with absolute paths.
pub fn read_skill(meta: &SkillMeta) -> std::io::Result<(String, Vec<String>)> {
    let body = std::fs::read_to_string(meta.dir.join("SKILL.md"))?;
    let mut files = Vec::new();
    let truncated = collect_files(&meta.dir, &meta.dir, &mut files, 0);
    files.sort();
    if truncated {
        files.push("[truncated: depth or file limit exceeded]".into());
    }
    Ok((body, files))
}

/// Returns true if the file list was truncated (depth > 4 or > 200 files).
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) -> bool {
    if depth > 4 || out.len() > 200 {
        return true;
    }
    let mut truncated = false;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if std::fs::symlink_metadata(&p)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                continue;
            }
            if p.is_dir() {
                truncated |= collect_files(root, &p, out, depth + 1);
            } else if p.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") || dir != root {
                out.push(p.display().to_string());
                if out.len() > 200 {
                    return true;
                }
            }
        }
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_skill(root: &Path, dir_name: &str, fm_name: Option<&str>, desc: &str) {
        let d = root.join(dir_name);
        std::fs::create_dir_all(d.join("scripts")).unwrap();
        let fm = match fm_name {
            Some(n) => format!(
                "---\nname: {n}\ndescription: {desc}\nlicense: X\n---\n\n# Body\nDo things.\n"
            ),
            None => format!("---\ndescription: {desc}\n---\n\n# Body\n"),
        };
        std::fs::write(d.join("SKILL.md"), fm).unwrap();
        std::fs::write(d.join("scripts").join("run.sh"), "#!/bin/bash\n").unwrap();
    }

    #[test]
    fn scan_search_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            tmp.path(),
            "council",
            Some("council-of-minds"),
            "Convene multiple model perspectives for hard decisions",
        );
        mk_skill(
            tmp.path(),
            "organizer",
            None,
            "Organize files by type and date",
        );
        std::fs::create_dir(tmp.path().join("not-a-skill")).unwrap(); // no SKILL.md

        let idx = SkillIndex::scan(&[tmp.path().display().to_string()]);
        assert_eq!(idx.skills.len(), 2);
        // frontmatter name wins; dir name is the fallback
        assert!(idx.get("council-of-minds").is_some());
        assert!(idx.get("organizer").is_some());

        let hits = idx.search("model perspectives decision", 5);
        assert_eq!(hits[0].name, "council-of-minds");

        let (body, files) = read_skill(idx.get("council-of-minds").unwrap()).unwrap();
        assert!(body.contains("Do things"));
        // Normalise separators: these are real filesystem paths, so on Windows
        // they come back as `scripts\run.sh`.
        assert!(
            files
                .iter()
                .any(|f| f.replace('\\', "/").ends_with("scripts/run.sh")),
            "expected the bundled script in the inventory: {files:?}"
        );
    }

    #[test]
    fn frontmatter_parses_name_description_version() {
        // NB: `version: 1.0` would be a YAML float and fail to deserialize
        // into Option<String>, taking the whole frontmatter with it. Real
        // skills use semver-ish strings, which YAML reads as strings.
        let fm = parse_frontmatter(
            "---\nname: a-skill\ndescription: does a thing\nversion: 1.2.0\n---\n\n# Body\n",
        )
        .expect("frontmatter should parse");
        assert_eq!(fm.name.as_deref(), Some("a-skill"));
        assert_eq!(fm.description, "does a thing");
        assert_eq!(fm.version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn frontmatter_without_a_name_still_parses() {
        // `scan` falls back to the directory name, so a nameless frontmatter
        // must yield Some(..) with name: None — not None.
        let fm = parse_frontmatter("---\ndescription: only a description\n---\n\nbody")
            .expect("frontmatter should parse");
        assert!(fm.name.is_none());
        assert_eq!(fm.description, "only a description");
    }

    #[test]
    fn frontmatter_survives_crlf_line_endings() {
        // A SKILL.md authored on Windows, or checked out with
        // core.autocrlf=true, has \r\n endings. The delimiter scan looks for
        // "\n---", which "\r\n---" still contains — the real risk is a stray
        // carriage return surviving inside the parsed VALUES and travelling
        // into descriptions the model reads and search indexes.
        let fm = parse_frontmatter(
            "---\r\nname: a-skill\r\ndescription: does a thing\r\nversion: 1.2.0\r\n---\r\n\r\n# Body\r\n",
        )
        .expect("CRLF frontmatter should parse");
        assert_eq!(fm.name.as_deref(), Some("a-skill"));
        assert_eq!(
            fm.description, "does a thing",
            "no stray carriage return in the description"
        );
        assert_eq!(fm.version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn frontmatter_absent_or_unterminated_yields_none() {
        assert!(parse_frontmatter("# No frontmatter at all\n").is_none());
        assert!(parse_frontmatter("").is_none());
        assert!(
            parse_frontmatter("---\nname: a\ndescription: d\n").is_none(),
            "an unclosed frontmatter block must not parse"
        );
    }

    #[test]
    fn frontmatter_closing_delimiter_tolerates_trailing_whitespace() {
        let fm = parse_frontmatter("---\nname: a\ndescription: d\n--- \n\nbody")
            .expect("trailing space after --- is still a delimiter");
        assert_eq!(fm.name.as_deref(), Some("a"));
    }

    #[test]
    fn frontmatter_stops_at_the_first_real_delimiter() {
        // A body containing its own --- (horizontal rules are common in
        // markdown) must not confuse the scan or truncate the description.
        let fm =
            parse_frontmatter("---\nname: a\ndescription: d\n---\n\n# Body\n\n---\n\nmore body\n")
                .expect("frontmatter should parse");
        assert_eq!(fm.name.as_deref(), Some("a"));
        assert_eq!(fm.description, "d");
    }

    #[test]
    fn missing_dir_is_reported_not_fatal() {
        let idx = SkillIndex::scan(&["/nonexistent/skills".into()]);
        assert!(idx.skills.is_empty());
        assert_eq!(idx.errors.len(), 1);
    }

    #[test]
    fn absorb_moves_skills_and_spares_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        let native = tmp.path().join("native");
        std::fs::create_dir_all(&native).unwrap();
        mk_skill(
            &native,
            "stray-skill",
            Some("stray-skill"),
            "dropped by a model",
        );
        std::fs::write(native.join("notes.txt"), "not a skill").unwrap();
        std::fs::create_dir(native.join("empty-dir")).unwrap();

        let (moved, errors) =
            absorb_into(&lib.display().to_string(), &[native.display().to_string()]);
        assert_eq!(moved, vec!["stray-skill"]);
        assert!(errors.is_empty());
        assert!(lib.join("stray-skill/SKILL.md").is_file());
        assert!(!native.join("stray-skill").exists());
        assert!(native.join("notes.txt").exists()); // junk untouched
        assert!(native.join("empty-dir").exists());

        // collision: same name appears again in native → skipped + reported
        mk_skill(&native, "stray-skill", Some("stray-skill"), "second copy");
        let (moved2, errors2) =
            absorb_into(&lib.display().to_string(), &[native.display().to_string()]);
        assert!(moved2.is_empty());
        assert_eq!(errors2.len(), 1);
        assert!(native.join("stray-skill").exists()); // left for the human
    }

    #[cfg(unix)]
    #[test]
    fn collect_files_skips_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(
            tmp.path(),
            "cyclical",
            Some("cyclical"),
            "tests symlink skip",
        );
        // a real bundled file
        std::fs::write(tmp.path().join("cyclical").join("real.sh"), "#!/bin/bash\n").unwrap();
        // a symlink to a file (should be skipped)
        symlink(
            tmp.path().join("cyclical").join("real.sh"),
            tmp.path().join("cyclical").join("link.sh"),
        )
        .unwrap();

        let idx = SkillIndex::scan(&[tmp.path().display().to_string()]);
        let (_body, files) = read_skill(idx.get("cyclical").unwrap()).unwrap();
        assert!(files.iter().any(|f| f.ends_with("real.sh")));
        assert!(
            !files.iter().any(|f| f.ends_with("link.sh")),
            "symlink leaked into inventory: {files:?}"
        );
    }
}
