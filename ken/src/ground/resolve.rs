//! Read a ground binding's source to a hashed span. The source produces bytes
//! (a file read, an allowlisted command's stdout, or a sandboxed generator's
//! stdout); the locator projects to a span; the span is hashed for the replay
//! binding. A pure predicate (judging) happens later, in the engine.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Config, Vcs};
use crate::ground::generator::{load_handler, GeneratorSrc, Sandbox};
use crate::schema::{GroundBinding, GroundSource, HandlerSource, Locator, SourceRef, SourceRoot};

/// A resolved span: the revision it was read at, the span's content hash, and
/// the span text.
#[derive(Debug, Clone)]
pub struct ResolvedSpan {
    pub rev: String,
    pub span_hash: String,
    pub text: String,
}

/// The result of reading a source + projecting its locator.
#[derive(Debug)]
pub enum ReadResult {
    /// A span was read (judge it with the predicate).
    Resolved(ResolvedSpan),
    /// The locator no longer resolves: an existence failure (-> `Refuted`).
    Unresolved,
    /// A transient read failure on a non-deterministic channel (-> `Inconclusive`).
    Transient,
    /// The source could not be read in a trustworthy way (-> `Errored`).
    Errored(String),
}

/// Build a ground source from a parsed source reference.
/// A named root that is handler-backed in config (a CURIE scheme with
/// `[sources.<scheme>] handler`) mounts code, capturing the handler's content
/// hash now; any other root is a plain file read.
///
/// # Errors
/// Returns an error if a handler-backed scheme's module cannot be loaded.
pub fn ground_source_for(
    config: &Config,
    store_root: &Path,
    src: SourceRef,
) -> crate::error::Result<GroundSource> {
    if let SourceRoot::Named(scheme) = &src.root {
        if let Some((src_path, caps)) = config.handler_for(scheme) {
            let (_, handler) = load_handler(store_root, scheme, &src_path, caps)?;
            return Ok(GroundSource::Handler(HandlerSource {
                handler,
                reference: src.path,
                rev: src.rev,
            }));
        }
    }
    Ok(GroundSource::File(src))
}

/// Read a binding's source and project its locator. `claim` is handed to a
/// generator via `KEN_VALUE`.
pub fn read_binding(
    config: &Config,
    store_root: &Path,
    binding: &GroundBinding,
    claim: &str,
) -> ReadResult {
    let (content, rev) = match read_source(config, store_root, &binding.source, claim) {
        Ok(pair) => pair,
        Err(ReadFail::Transient) => return ReadResult::Transient,
        Err(ReadFail::Errored(m)) => return ReadResult::Errored(m),
    };
    match resolve_locator(&binding.locator, &content) {
        Err(unsupported) => ReadResult::Errored(unsupported),
        Ok(None) => ReadResult::Unresolved,
        Ok(Some(text)) => {
            let span_hash = blake3::hash(text.as_bytes()).to_hex().to_string();
            ReadResult::Resolved(ResolvedSpan {
                rev,
                span_hash,
                text,
            })
        }
    }
}

enum ReadFail {
    Transient,
    Errored(String),
}

/// Read the raw bytes of a source, plus the revision label to record.
fn read_source(
    config: &Config,
    store_root: &Path,
    source: &GroundSource,
    claim: &str,
) -> Result<(String, String), ReadFail> {
    match source {
        GroundSource::File(src) => {
            let (root_path, vcs) = config
                .resolve_root(&src.root, store_root)
                .map_err(|e| ReadFail::Errored(e.to_string()))?;
            let content = read_file(&root_path, &src.path, src.rev.as_deref(), vcs)?;
            Ok((
                content,
                src.rev.clone().unwrap_or_else(|| "working".to_string()),
            ))
        }
        GroundSource::Command(cmd) => {
            let program = cmd
                .argv
                .first()
                .ok_or_else(|| ReadFail::Errored("empty command".into()))?;
            if !config.command.allows(program) {
                return Err(ReadFail::Errored(format!(
                    "command `{program}` not in [command] allow list"
                )));
            }
            let (root_path, _) = config
                .resolve_root(&cmd.root, store_root)
                .map_err(|e| ReadFail::Errored(e.to_string()))?;
            let out = run_command(&cmd.argv, &root_path, config.sandbox.timeout_secs())?;
            Ok((out, "computed".to_string()))
        }
        GroundSource::Generator(gref) => {
            let registry = crate::ground::generator::GeneratorRegistry::new(store_root);
            let src_text = registry
                .read_source(gref)
                .map_err(|e| ReadFail::Errored(e.to_string()))?;
            let src = GeneratorSrc {
                name: gref.name.clone(),
                source: src_text,
                caps: gref.caps.clone(),
                cost_estimate: gref.cost_estimate,
            };
            let sandbox = Sandbox::new(
                config.sandbox.runtime.clone(),
                Duration::from_secs_f64(config.sandbox.timeout_secs()),
            );
            let run = sandbox.run(&src, Some(&gref.hash), claim);
            if run.hash_changed {
                return Err(ReadFail::Errored(
                    "generator source or capabilities changed; update its binding".into(),
                ));
            }
            if run.timed_out {
                return Err(ReadFail::Transient);
            }
            if !run.ok() {
                return Err(ReadFail::Errored("generator did not exit cleanly".into()));
            }
            Ok((run.stdout, "computed".to_string()))
        }
        GroundSource::Handler(h) => {
            let scheme = &h.handler.scheme;
            // Rebuild from the *current* config caps and live source, so drift
            // in either is a loud diff against the recorded hash.
            let (src_path, caps) = config.handler_for(scheme).ok_or_else(|| {
                ReadFail::Errored(format!(
                    "source root `{scheme}` is no longer handler-backed"
                ))
            })?;
            let (src, _) = load_handler(store_root, scheme, &src_path, caps)
                .map_err(|e| ReadFail::Errored(e.to_string()))?;
            let sandbox = Sandbox::new(
                config.sandbox.runtime.clone(),
                Duration::from_secs_f64(config.sandbox.timeout_secs()),
            );
            let run =
                sandbox.run_handler(&src, Some(&h.handler.hash), &h.reference, h.rev.as_deref());
            if run.hash_changed {
                return Err(ReadFail::Errored(format!(
                    "handler `{scheme}` source or capabilities changed; update its binding"
                )));
            }
            if run.timed_out {
                return Err(ReadFail::Transient);
            }
            if !run.ok() {
                return Err(ReadFail::Errored(format!(
                    "handler `{scheme}` did not exit cleanly"
                )));
            }
            Ok((
                run.stdout,
                h.rev.clone().unwrap_or_else(|| "computed".to_string()),
            ))
        }
    }
}

fn read_file(root: &Path, path: &str, rev: Option<&str>, vcs: Vcs) -> Result<String, ReadFail> {
    match rev {
        None => std::fs::read_to_string(root.join(path))
            .map_err(|e| ReadFail::Errored(format!("{path}: {e}"))),
        Some(rev) => {
            let output = match vcs {
                Vcs::Jj => Command::new("jj")
                    .args([
                        "-R",
                        root.to_str().unwrap_or("."),
                        "file",
                        "show",
                        "-r",
                        rev,
                        path,
                    ])
                    .output(),
                Vcs::Git => Command::new("git")
                    .args([
                        "-C",
                        root.to_str().unwrap_or("."),
                        "show",
                        &format!("{rev}:{path}"),
                    ])
                    .output(),
            }
            .map_err(|e| ReadFail::Errored(format!("spawning vcs: {e}")))?;
            if !output.status.success() {
                return Err(ReadFail::Errored(
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                ));
            }
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
    }
}

/// Run an allowlisted command in `root`, with a hard timeout. Non-zero exit or
/// spawn failure is `Errored`; a timeout is `Transient`.
fn run_command(argv: &[String], root: &Path, timeout_secs: f64) -> Result<String, ReadFail> {
    let digest = blake3::hash(argv.join(" ").as_bytes()).to_hex();
    let stdout_path = std::env::temp_dir().join(format!("ken-cmd-{}.out", &digest[..16]));
    let stdout_file =
        std::fs::File::create(&stdout_path).map_err(|e| ReadFail::Errored(format!("temp: {e}")))?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ReadFail::Errored(format!("spawning {}: {e}", argv[0])))?;

    let start = Instant::now();
    let timeout = Duration::from_secs_f64(timeout_secs.max(1.0));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&stdout_path);
                    return Err(ReadFail::Transient);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&stdout_path);
                return Err(ReadFail::Errored(e.to_string()));
            }
        }
    };
    let out = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let _ = std::fs::remove_file(&stdout_path);
    if !status.success() {
        return Err(ReadFail::Errored(format!(
            "command exited with {}",
            status
                .code()
                .map_or_else(|| "signal".into(), |c| c.to_string())
        )));
    }
    Ok(out)
}

/// Project a locator over content. `Ok(None)` = does not resolve;
/// `Err(msg)` = unsupported locator (tree-sitter).
fn resolve_locator(locator: &Locator, content: &str) -> Result<Option<String>, String> {
    match locator {
        Locator::Whole => Ok(Some(content.to_string())),
        Locator::LineRange { start, end } => Ok(line_range(content, *start, *end)),
        Locator::Quote { needle } => Ok(content.contains(needle.as_str()).then(|| needle.clone())),
        Locator::Heading { heading } => Ok(markdown_section(content, heading)),
        Locator::TreeSitter { lang, .. } => Err(format!(
            "tree-sitter ({lang}) deferred; use a heading, quote, or line-range locator for now"
        )),
    }
}

fn line_range(content: &str, start: usize, end: usize) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    if start == 0 || start > lines.len() {
        return None;
    }
    let end = end.min(lines.len());
    Some(lines[start - 1..end].join("\n"))
}

/// Extract a Markdown section: the heading line plus everything up to the next
/// heading of the same or higher level (case-insensitive on the trimmed text).
fn markdown_section(content: &str, heading: &str) -> Option<String> {
    let want = heading.trim().to_ascii_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let mut start = None;
    let mut level = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if let Some((lvl, text)) = parse_heading(line) {
            if text.to_ascii_lowercase() == want {
                start = Some(i);
                level = lvl;
                break;
            }
        }
    }
    let start = start?;
    let mut end = lines.len();
    for (j, line) in lines.iter().enumerate().skip(start + 1) {
        if let Some((lvl, _)) = parse_heading(line) {
            if lvl <= level {
                end = j;
                break;
            }
        }
    }
    Some(lines[start..end].join("\n"))
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 {
        return None;
    }
    let rest = trimmed[hashes..].trim();
    if rest.is_empty() {
        return None;
    }
    Some((hashes, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str =
        "# Title\nintro\n\n## Authentication\nauth lives in src/auth.rs\n\n## Other\nx";

    #[test]
    fn markdown_section_extracts_until_next_same_level() {
        let s = markdown_section(DOC, "authentication").unwrap();
        assert!(s.contains("auth lives in src/auth.rs"));
        assert!(!s.contains("## Other"));
        assert!(s.starts_with("## Authentication"));
    }

    #[test]
    fn line_range_slices_inclusive() {
        let s = line_range("a\nb\nc\nd", 2, 3).unwrap();
        assert_eq!(s, "b\nc");
        assert!(line_range("a\nb", 5, 6).is_none());
    }

    #[test]
    fn quote_present_or_absent() {
        assert_eq!(
            resolve_locator(
                &Locator::Quote {
                    needle: "verify".into()
                },
                "fn verify() {}"
            )
            .unwrap(),
            Some("verify".to_string())
        );
        assert_eq!(
            resolve_locator(
                &Locator::Quote {
                    needle: "nope".into()
                },
                "fn verify() {}"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn treesitter_is_unsupported() {
        let r = resolve_locator(
            &Locator::TreeSitter {
                lang: "rust".into(),
                query: "(x)".into(),
            },
            "fn x() {}",
        );
        assert!(r.is_err());
    }
}
