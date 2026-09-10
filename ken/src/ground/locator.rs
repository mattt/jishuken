//! Parse the README locator strings to a `(SourceRef, Locator)` and render them
//! back for `why`/`recall`.
//! Grammar:
//!
//! - `path`                      whole file
//! - `path#heading`              Markdown section
//! - `path?q="needle"`           quoted substring
//! - `path#L40-58`               line range
//! - `path#ts:(query)`           tree-sitter match (parsed; resolution deferred)
//! - `<root>:` prefix            a named source root (`wiki:...`), or reserved `store:` / `project:`
//! - `@<rev>` suffix             a pinned revision (also settable via `--rev`)

use crate::error::{Error, Result};
use crate::schema::{GroundBinding, GroundSource, Locator, SourceRef, SourceRoot};

/// Parse a locator string into a source reference and a locator. `rev` from a
/// `--rev` flag wins over any inline `@rev`.
///
/// # Errors
/// Returns an error if the input is empty or contains a malformed line range.
pub fn parse_source(input: &str, rev: Option<String>) -> Result<(SourceRef, Locator)> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::Config("empty source locator".into()));
    }

    // Optional inline `@rev` suffix (only the last `@`, and only if no `/`
    // follows, so paths with `@` are not eaten).
    let (body, inline_rev) = match input.rsplit_once('@') {
        Some((b, r)) if !r.contains('/') && !r.is_empty() => {
            (b.trim_end(), Some(r.trim().to_string()))
        }
        _ => (input, None),
    };
    let rev = rev.or(inline_rev);

    // Leading `<root>:` named-root prefix. A bare identifier followed by `:`
    // and not `//` (networked schemes are deferred).
    let (root, rest) = split_root(body);

    // `?q=` quote locator.
    if let Some((path, q)) = rest.split_once("?q=") {
        let needle = q.trim().trim_matches('"').to_string();
        return Ok((source_ref(root, path, rev), Locator::Quote { needle }));
    }

    // `#fragment` locators, else whole file.
    let (path, frag) = match rest.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (rest, None),
    };
    let locator = match frag {
        None => Locator::Whole,
        Some(f) if f.starts_with("ts:") => {
            let query = f[3..].to_string();
            Locator::TreeSitter {
                lang: lang_for(path),
                query,
            }
        }
        Some(f) if is_line_range(f) => parse_line_range(f)?,
        Some(f) => Locator::Heading {
            heading: f.to_string(),
        },
    };
    Ok((source_ref(root, path, rev), locator))
}

fn source_ref(root: SourceRoot, path: &str, rev: Option<String>) -> SourceRef {
    SourceRef {
        root,
        path: path.to_string(),
        rev,
    }
}

fn split_root(body: &str) -> (SourceRoot, &str) {
    // A named root is `<ident>:` where ident is [a-z0-9_-]+ and the next char
    // is not `/` (so `https://` stays a path, networked deferred).
    if let Some(colon) = body.find(':') {
        let (name, after) = body.split_at(colon);
        let rest = &after[1..];
        let ident = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        if ident && !rest.starts_with('/') {
            let root = match name {
                "store" => SourceRoot::Store,
                "project" => SourceRoot::Project,
                other => SourceRoot::Named(other.to_string()),
            };
            return (root, rest);
        }
    }
    (SourceRoot::Project, body)
}

fn is_line_range(frag: &str) -> bool {
    frag.starts_with('L') && frag[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn parse_line_range(frag: &str) -> Result<Locator> {
    let nums = &frag[1..];
    let (start, end) = match nums.split_once('-') {
        Some((a, b)) => (a, b),
        None => (nums, nums),
    };
    let start: usize = start
        .parse()
        .map_err(|_| Error::Config(format!("bad line range: {frag}")))?;
    let end: usize = end
        .parse()
        .map_err(|_| Error::Config(format!("bad line range: {frag}")))?;
    if start == 0 || end < start {
        return Err(Error::Config(format!("bad line range: {frag}")));
    }
    Ok(Locator::LineRange { start, end })
}

fn lang_for(path: &str) -> String {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("rs") => "rust",
        Some("md" | "markdown") => "markdown",
        Some("ts" | "tsx") => "typescript",
        Some("js" | "jsx") => "javascript",
        Some("py") => "python",
        Some("go") => "go",
        _ => "text",
    }
    .to_string()
}

/// Render a ground binding's source to its human string form, shared by `why`,
/// `recall`, the MCP surface, and the tagged op-log label.
pub fn render_ground(binding: &GroundBinding) -> String {
    match &binding.source {
        GroundSource::File(src) => render_source(src, &binding.locator),
        GroundSource::Command(cmd) => format!("$ {}", cmd.argv.join(" ")),
        GroundSource::Generator(gr) => format!("gen {}", gr.display()),
        GroundSource::Handler(h) => render_source(
            &SourceRef {
                root: SourceRoot::Named(h.handler.scheme.clone()),
                path: h.reference.clone(),
                rev: h.rev.clone(),
            },
            &binding.locator,
        ),
    }
}

/// Render a `(SourceRef, Locator)` back to its string form, for `why`/`recall`.
pub fn render_source(src: &SourceRef, loc: &Locator) -> String {
    let prefix = match &src.root {
        SourceRoot::Project => String::new(),
        SourceRoot::Store => "store:".to_string(),
        SourceRoot::Named(n) => format!("{n}:"),
    };
    let frag = match loc {
        Locator::Whole => String::new(),
        Locator::Heading { heading } => format!("#{heading}"),
        Locator::Quote { needle } => format!("?q=\"{needle}\""),
        Locator::LineRange { start, end } => {
            if start == end {
                format!("#L{start}")
            } else {
                format!("#L{start}-{end}")
            }
        }
        Locator::TreeSitter { query, .. } => format!("#ts:{query}"),
    };
    let rev = src
        .rev
        .as_ref()
        .map(|r| format!(" @ {r}"))
        .unwrap_or_default();
    format!("{prefix}{}{frag}{rev}", src.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_heading_with_named_root() {
        let (src, loc) = parse_source("wiki:Architecture.md#authentication", None).unwrap();
        assert_eq!(src.root, SourceRoot::Named("wiki".into()));
        assert_eq!(src.path, "Architecture.md");
        assert_eq!(
            loc,
            Locator::Heading {
                heading: "authentication".into()
            }
        );
    }

    #[test]
    fn parses_line_range() {
        let (src, loc) = parse_source("src/router.rs#L40-58", None).unwrap();
        assert_eq!(src.root, SourceRoot::Project);
        assert_eq!(loc, Locator::LineRange { start: 40, end: 58 });
    }

    #[test]
    fn parses_quote_and_whole() {
        let (_, q) = parse_source("Doc.md?q=\"verify_token\"", None).unwrap();
        assert_eq!(
            q,
            Locator::Quote {
                needle: "verify_token".into()
            }
        );
        let (_, w) = parse_source("src/main.rs", None).unwrap();
        assert_eq!(w, Locator::Whole);
    }

    #[test]
    fn parses_treesitter_lang_from_extension() {
        let (_, loc) = parse_source("src/auth.rs#ts:(function_item)", None).unwrap();
        assert_eq!(
            loc,
            Locator::TreeSitter {
                lang: "rust".into(),
                query: "(function_item)".into()
            }
        );
    }

    #[test]
    fn rev_from_flag_and_inline() {
        let (src, _) = parse_source("wiki:Doc.md#h", Some("main".into())).unwrap();
        assert_eq!(src.rev.as_deref(), Some("main"));
        let (src2, _) = parse_source("wiki:Doc.md#h@9f12", None).unwrap();
        assert_eq!(src2.rev.as_deref(), Some("9f12"));
    }

    #[test]
    fn round_trips_through_render() {
        for s in [
            "wiki:Architecture.md#authentication",
            "src/router.rs#L40-58",
            "src/main.rs",
        ] {
            let (src, loc) = parse_source(s, None).unwrap();
            let rendered = render_source(&src, &loc);
            let (src2, loc2) = parse_source(&rendered, None).unwrap();
            assert_eq!((src, loc), (src2, loc2), "round trip failed for {s}");
        }
    }
}
