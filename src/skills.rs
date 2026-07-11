//! Skills on demand — the same disease ondemand-mcp cures for tools, cured
//! for agent skills. opencode (and Claude) load every skill's name and
//! description into the system context at startup: 36 skills measured at
//! ~9.2 KB ≈ 2,300 tokens per session, used or not. Behind the gateway the
//! cost is two small meta-tools, and the library can grow without bound.
//!
//! A skill is a directory with a SKILL.md whose YAML frontmatter carries
//! `name` and `description`. `read_skill` returns the full body plus the
//! skill's file tree (absolute paths), so the agent can run bundled scripts
//! via its shell exactly as a natively-loaded skill would instruct.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
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
}

fn parse_frontmatter(text: &str) -> Option<Frontmatter> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    serde_norway::from_str(&rest[..end]).ok()
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
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
                let Ok(text) = std::fs::read_to_string(&skill_md) else { continue };
                let fm = parse_frontmatter(&text);
                let name = fm
                    .as_ref()
                    .and_then(|f| f.name.clone())
                    .unwrap_or_else(|| {
                        path.file_name().unwrap_or_default().to_string_lossy().to_string()
                    });
                let description = fm.map(|f| f.description).unwrap_or_default();
                idx.skills.push(SkillMeta { name, description, dir: path });
            }
        }
        idx.skills.sort_by(|a, b| a.name.cmp(&b.name));
        idx
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<&SkillMeta> {
        let q = query.to_lowercase();
        let tokens: Vec<&str> = q.split_whitespace().filter(|t| t.len() > 1).collect();
        let mut hits: Vec<(i64, &SkillMeta)> = Vec::new();
        for s in &self.skills {
            let name = s.name.to_lowercase();
            let desc = s.description.to_lowercase();
            let mut score = 0i64;
            if name == q {
                score += 100;
            } else if name.contains(&q) {
                score += 60;
            }
            for t in &tokens {
                if name.contains(t) {
                    score += 20;
                }
                if desc.contains(t) {
                    score += 8;
                }
            }
            if score > 0 {
                hits.push((score, s));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
        hits.into_iter().take(limit).map(|(_, s)| s).collect()
    }

    pub fn get(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.iter().find(|s| s.name == name)
    }
}

/// SKILL.md body + the skill's file tree with absolute paths.
pub fn read_skill(meta: &SkillMeta) -> std::io::Result<(String, Vec<String>)> {
    let body = std::fs::read_to_string(meta.dir.join("SKILL.md"))?;
    let mut files = Vec::new();
    collect_files(&meta.dir, &meta.dir, &mut files, 0);
    files.sort();
    Ok((body, files))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 4 || out.len() > 200 {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_files(root, &p, out, depth + 1);
            } else if p.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") || dir != root {
                out.push(p.display().to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_skill(root: &Path, dir_name: &str, fm_name: Option<&str>, desc: &str) {
        let d = root.join(dir_name);
        std::fs::create_dir_all(d.join("scripts")).unwrap();
        let fm = match fm_name {
            Some(n) => format!("---\nname: {n}\ndescription: {desc}\nlicense: X\n---\n\n# Body\nDo things.\n"),
            None => format!("---\ndescription: {desc}\n---\n\n# Body\n"),
        };
        std::fs::write(d.join("SKILL.md"), fm).unwrap();
        std::fs::write(d.join("scripts").join("run.sh"), "#!/bin/bash\n").unwrap();
    }

    #[test]
    fn scan_search_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        mk_skill(tmp.path(), "council", Some("council-of-minds"),
                 "Convene multiple model perspectives for hard decisions");
        mk_skill(tmp.path(), "organizer", None, "Organize files by type and date");
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
        assert!(files.iter().any(|f| f.ends_with("scripts/run.sh")));
    }

    #[test]
    fn missing_dir_is_reported_not_fatal() {
        let idx = SkillIndex::scan(&["/nonexistent/skills".into()]);
        assert!(idx.skills.is_empty());
        assert_eq!(idx.errors.len(), 1);
    }
}
