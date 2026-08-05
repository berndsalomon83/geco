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
///
/// Symlinks are SKIPPED, not followed. `absorb_into`'s primary path is
/// `rename`, which moves a symlink as a symlink; this fallback runs only when
/// the absorb dir and library are on different filesystems, and absorbed
/// content is only semi-trusted (a skill can be dropped by a model, an
/// installer, or a playbook). Deciding dir-vs-file with the entry's OWN file
/// type instead of `path().is_dir()` — which follows the link — stops a
/// symlink inside a skill from (a) copying data from OUTSIDE the tree into the
/// library, verified by repro, and (b) recursing forever around a symlink
/// loop and overflowing the stack. A self-contained skill has no need of
/// symlinks, so dropping them is safe and predictable.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let p = entry.path();
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
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
    // A UTF-8 BOM (Windows Notepad, PowerShell 5.1 Out-File) is invisible in
    // every editor but makes the file no longer START with "---", which
    // silently discarded the whole frontmatter. Same class as the CRLF
    // handling below: bytes Windows tooling adds, authors can't see.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
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
        // Parse failed. The most common cause is an unquoted scalar value
        // containing ": " (colon-space), which YAML reads as a nested
        // mapping ("mapping values are not allowed in this context"). A
        // SKILL.md description like "phases: initiation, elongation" hits
        // this and the skill silently loses its description. Retry with
        // every plain-scalar value line double-quoted, which makes the
        // colons literal. Only reached on failure, so the happy path is
        // untouched.
        let quoted = quote_plain_scalar_values(candidate);
        if quoted != candidate {
            if let Ok(fm) = serde_norway::from_str::<Frontmatter>(&quoted) {
                return Some(fm);
            }
        }
        // Parse failed — this --- was inside a block scalar. Keep searching.
        search_from = abs + 4;
    }
}

/// Double-quote any `key: value` line whose value is an unquoted scalar
/// containing ": " (colon-space). Leaves already-quoted, block-scalar, or
/// colon-free values alone. Used only as a retry after a strict parse fails,
/// to rescue SKILL.md frontmatter whose description has a mid-sentence colon.
///
/// It rewrites only lines at mapping level. A `|` or `>` block scalar opens a
/// region whose lines are prose, not `key: value` pairs, and quoting one of
/// them puts two literal `"` characters into the description the model reads.
/// The block-scalar author is exactly the author who had colons to escape, so
/// the two shapes turn up together.
fn quote_plain_scalar_values(frontmatter: &str) -> String {
    // Indent of the key that opened a block scalar, if we are inside one.
    // Everything indented past it belongs to the block.
    let mut block_indent: Option<usize> = None;
    frontmatter
        .lines()
        .map(|line| {
            let indent = line.len() - line.trim_start().len();
            if let Some(open) = block_indent {
                // A blank line inside a block scalar is content, not the end.
                if line.trim().is_empty() || indent > open {
                    return line.to_string();
                }
                block_indent = None;
            }
            // Skip lines that are already quoted or use a block scalar
            // indicator, and lines with no `key: value` structure.
            let Some((key, val)) = line.split_once(':') else {
                return line.to_string();
            };
            // `trim`, not `trim_start`: on a CRLF file every value ends in a
            // carriage return, which inside the quotes we are about to add is
            // a literal character rather than a line ending.
            let val = val.trim();
            if val.starts_with('|') || val.starts_with('>') {
                block_indent = Some(indent);
                return line.to_string();
            }
            if val.is_empty() || val.starts_with('"') || val.starts_with('\'') {
                return line.to_string();
            }
            // Needs quoting when the value contains ": " — or ENDS in a bare
            // colon ("description: Steps to follow:"), which YAML rejects
            // with the same "mapping values are not allowed" error but which
            // contains no colon-space, so the old check left the line
            // byte-identical and the retry never reparsed.
            if !val.contains(": ") && !val.ends_with(':') {
                return line.to_string();
            }
            // A plain scalar ends at " #" — YAML would have dropped the rest as
            // a comment, and swallowing it into the quotes would make the retry
            // disagree with the strict path about what the value even is.
            let val = match val.find(" #") {
                Some(i) => val[..i].trim_end(),
                None => val,
            };
            // Double-quote, escaping inner quotes and backslashes.
            let escaped = val.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{key}: \"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cross-filesystem fallback for `rename` in `absorb_into`. Refuses a
/// symlinked skill root outright: `read_dir` on the ROOT follows the link
/// even though `copy_dir_recursive` skips symlink ENTRIES below it, so
/// copying would pull the TARGET's tree into the library — and the
/// `remove_dir_all` after it would delete only the link, silently forking
/// the user's real skill source. `rename` (the primary path) moves a symlink
/// as a symlink, which is the intended behavior; crossing a filesystem
/// boundary must not change what absorb does to a link.
fn copy_fallback(src: &Path, dst: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(src)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "skill dir is a symlink; refusing to copy its target across filesystems",
        ));
    }
    copy_dir_recursive(src, dst)
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
                    if let Err(copy_err) = copy_fallback(&src, &dst) {
                        // Clear the half-written destination. Left in place it
                        // makes `dst.exists()` true forever, so every later
                        // absorb reports "already in library (resolve
                        // manually)" — pointing the user at a collision with
                        // debris this function created, while the real skill
                        // stays stranded in the native dir costing context.
                        let _ = std::fs::remove_dir_all(&dst);
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
        // Same name in two configured dirs: without this, search ranked ALL
        // copies (a model could be sold on the second dir's description)
        // while `get` resolved the FIRST — so read_skill served a different
        // body than the one search advertised. Keep exactly the copy `get`
        // returns (the stable sort preserves dir order among equal names, so
        // first = earlier configured dir) and say so, instead of silently
        // indexing both. dedup_by hands the closure (removal candidate,
        // kept-previous) over consecutive elements, which sorting guarantees.
        let SkillIndex { skills, errors } = &mut idx;
        skills.dedup_by(|dup, kept| {
            let same = dup.name == kept.name;
            if same {
                errors.push(format!(
                    "duplicate skill name '{}': using {}, ignoring {}",
                    dup.name,
                    kept.dir.display(),
                    dup.dir.display()
                ));
            }
            same
        });
        idx
    }

    /// Ranked by the same scorer as tool search, so the two surfaces behave
    /// alike. Skills have no server field.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&SkillMeta> {
        self.search_with(query, limit, &[])
    }

    /// `search` with user-provided `extra` synonym groups, so a configured
    /// `proxy.search_synonyms` reaches skill discovery too.
    pub fn search_with(&self, query: &str, limit: usize, extra: &[Vec<String>]) -> Vec<&SkillMeta> {
        let docs: Vec<crate::search::Doc> = self
            .skills
            .iter()
            .map(|s| crate::search::Doc {
                name: &s.name,
                description: &s.description,
                server: None,
            })
            .collect();
        crate::search::rank_with(&docs, query, limit, extra)
            .into_iter()
            .map(|r| &self.skills[r.index])
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
    fn frontmatter_description_with_colon_space_is_rescued() {
        // An unquoted scalar value containing ": " (colon-space) is valid
        // prose but invalid YAML — "mapping values are not allowed in this
        // context". Without the quote-retry the skill silently loses its
        // description and becomes invisible to search. This is the exact
        // case found by the 50-skill stress test (stress-38: "phases:
        // initiation, elongation").
        let fm = parse_frontmatter(
            "---\nname: stress-38\ndescription: Animated walkthrough of ribosome translation phases: initiation, elongation, and termination at codon resolution.\n---\n\n# Body\n",
        )
        .expect("colon-space description must parse via the quote-retry");
        assert_eq!(fm.name.as_deref(), Some("stress-38"));
        assert_eq!(
            fm.description,
            "Animated walkthrough of ribosome translation phases: initiation, elongation, and termination at codon resolution."
        );
    }

    #[test]
    fn frontmatter_description_with_colon_space_and_quotes_is_escaped() {
        // The quote-retry must escape inner double quotes and backslashes,
        // not just wrap the value. A description that already contains a
        // quoted phrase round-trips intact.
        let fm = parse_frontmatter(
            "---\nname: q\ndescription: She said \"hello: world\" and left C:\\Users\\x\n---\n\n# Body\n",
        )
        .expect("colon-space with quotes must parse and escape");
        assert_eq!(
            fm.description,
            "She said \"hello: world\" and left C:\\Users\\x"
        );
    }

    /// The retry must not damage what it is rescuing. A block scalar's lines
    /// are prose; the rewriter used to treat any of them containing a second
    /// ": " as a `key: value` pair and wrap it in quotes, so the description
    /// the model reads came back with two literal `"` in the middle of a
    /// sentence. The trigger is ordinary: one sibling key with a colon is
    /// enough to fail the strict parse and send the whole block through here.
    #[test]
    fn the_retry_leaves_block_scalar_prose_alone() {
        let fm = parse_frontmatter(
            "---\nname: pdf\ndescription: |\n  Supports: AcroForm: yes.\nlicense: see LICENSE: MIT\n---\n\n# Body\n",
        )
        .expect("the sibling colon must still be rescued");
        assert_eq!(
            fm.description, "Supports: AcroForm: yes.\n",
            "block-scalar content must survive byte for byte"
        );
    }

    /// CRLF is what a SKILL.md written on Windows looks like. The carriage
    /// return sits at the end of the value, and inside the quotes the retry
    /// adds it stops being a line ending and becomes part of the description.
    #[test]
    fn a_crlf_file_does_not_smuggle_a_carriage_return_into_the_value() {
        let fm = parse_frontmatter(
            "---\r\nname: crlf\r\ndescription: phases: one, two\r\n---\r\n\r\n# Body\r\n",
        )
        .expect("CRLF frontmatter must parse");
        assert_eq!(fm.description, "phases: one, two");
    }

    /// A trailing `#` comment is not part of the value. The strict path drops
    /// it; before this the retry path kept it, so the same SKILL.md described
    /// itself differently depending on whether it happened to parse.
    #[test]
    fn a_trailing_comment_is_not_swallowed_into_the_value() {
        let fm = parse_frontmatter(
            "---\nname: c\ndescription: phases: one, two # internal note\n---\n\n# Body\n",
        )
        .expect("must parse via the retry");
        assert_eq!(fm.description, "phases: one, two");
    }

    /// A value ENDING in a bare colon fails YAML the same way colon-space
    /// does, but contains no ": " — so the old retry produced a byte-identical
    /// line and never reparsed. Both the description itself and a sibling key
    /// with a trailing colon (which would take the whole frontmatter down with
    /// it) must be rescued.
    #[test]
    fn frontmatter_value_ending_in_a_bare_colon_is_rescued() {
        let fm =
            parse_frontmatter("---\nname: colon\ndescription: Steps to follow:\n---\n\n# Body\n")
                .expect("a trailing bare colon must parse via the quote-retry");
        assert_eq!(fm.description, "Steps to follow:");

        let fm = parse_frontmatter(
            "---\nname: colon2\ndescription: fine on its own\nnote: remember this:\n---\n\n# Body\n",
        )
        .expect("a sibling key's trailing colon must not kill the frontmatter");
        assert_eq!(fm.description, "fine on its own");
    }

    /// A UTF-8 BOM (Notepad, PowerShell 5.1 Out-File) is invisible to the
    /// author but made the file no longer start with "---", silently
    /// discarding the entire frontmatter — dir-name fallback, empty
    /// description, nothing in idx.errors.
    #[test]
    fn frontmatter_survives_a_utf8_bom() {
        let fm = parse_frontmatter(
            "\u{feff}---\nname: bom-skill\ndescription: authored in notepad\n---\n\n# Body\n",
        )
        .expect("a BOM before the opening --- must not discard the frontmatter");
        assert_eq!(fm.name.as_deref(), Some("bom-skill"));
        assert_eq!(fm.description, "authored in notepad");
    }

    /// Two configured dirs, same skill name: search must not be able to rank
    /// a copy that `get` (and therefore read_skill) will never serve. The
    /// earlier dir wins, the loser is dropped from the index, and the
    /// collision is reported instead of silently indexing both.
    #[test]
    fn duplicate_skill_names_keep_only_the_copy_get_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let lib1 = tmp.path().join("lib1");
        let lib2 = tmp.path().join("lib2");
        mk_skill(
            &lib1,
            "dup",
            Some("dup"),
            "kubernetes cluster administration",
        );
        mk_skill(&lib2, "dup", Some("dup"), "baking sourdough bread at home");

        let idx = SkillIndex::scan(&[lib1.display().to_string(), lib2.display().to_string()]);
        assert_eq!(
            idx.skills.len(),
            1,
            "one copy survives, got {:?}",
            idx.skills
        );
        assert!(idx.get("dup").unwrap().dir.starts_with(&lib1));
        assert!(
            idx.errors
                .iter()
                .any(|e| e.contains("duplicate skill name 'dup'")),
            "the collision must be reported: {:?}",
            idx.errors
        );
        // The copy search ranks is the copy read_skill serves.
        let hits = idx.search("baking sourdough bread", 5);
        for h in hits {
            assert!(
                h.dir.starts_with(&lib1),
                "search surfaced the dropped copy: {:?}",
                h.dir
            );
        }
    }

    #[test]
    fn missing_dir_is_reported_not_fatal() {
        let idx = SkillIndex::scan(&["/nonexistent/skills".into()]);
        assert!(idx.skills.is_empty());
        assert_eq!(idx.errors.len(), 1);
    }

    /// The cross-filesystem copy fallback must not follow a symlink out of the
    /// skill it is copying — doing so pulled data from OUTSIDE the tree into the
    /// library (verified by repro) and could loop forever around a link cycle.
    /// This drives `copy_dir_recursive` directly, because forcing an EXDEV
    /// rename inside a test is not portable.
    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_does_not_follow_symlinks_out_of_the_tree() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "not part of any skill").unwrap();

        let skill = tmp.path().join("skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: s\ndescription: d\n---\n",
        )
        .unwrap();
        std::fs::write(skill.join("real.txt"), "a genuine bundled file").unwrap();
        symlink(&outside, skill.join("link_to_outside")).unwrap();

        let dst = tmp.path().join("copied");
        copy_dir_recursive(&skill, &dst).expect("copy must succeed, skipping the link");

        assert!(dst.join("SKILL.md").is_file(), "real files are copied");
        assert!(
            dst.join("real.txt").is_file(),
            "real bundled files are copied"
        );
        assert!(
            !dst.join("link_to_outside").exists(),
            "the symlink must be skipped, not recreated"
        );
        assert!(
            !dst.join("link_to_outside").join("secret.txt").exists(),
            "data from outside the skill must never reach the library"
        );
    }

    /// Same defect class one level UP: `read_dir` on a symlinked ROOT follows
    /// the link even though entries below it are checked, so the fallback
    /// would copy the target's tree and absorb would then delete only the
    /// link — forking the user's real skill source. The fallback must refuse
    /// instead. Driven directly for the same portability reason as above.
    #[cfg(unix)]
    #[test]
    fn copy_fallback_refuses_a_symlinked_skill_root() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real-skill");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("SKILL.md"), "---\nname: s\ndescription: d\n---\n").unwrap();

        let link = tmp.path().join("linked-skill");
        symlink(&real, &link).unwrap();

        let dst = tmp.path().join("copied");
        copy_fallback(&link, &dst).expect_err("a symlinked root must be refused, not dereferenced");
        assert!(!dst.exists(), "nothing may be created for a refused copy");
        assert!(
            real.join("SKILL.md").is_file(),
            "the real source must be untouched"
        );
        // A regular directory still copies through the same entry point.
        copy_fallback(&real, &dst).expect("a real dir must still copy");
        assert!(dst.join("SKILL.md").is_file());
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
