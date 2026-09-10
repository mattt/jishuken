//! `ken.toml` parsing.
//! Lives in the store root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::duration::{FixedDuration, HalfLife};
use crate::error::{Error, Result};
use crate::schema::{Capabilities, SourceRoot};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub decay: Decay,
    /// Read old class mappings when opening pre-release stores.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) volatility: BTreeMap<String, HalfLife>,
    #[serde(default)]
    pub sandbox: Sandbox,
    #[serde(default)]
    pub daemon: Daemon,
    #[serde(default)]
    pub command: CommandCfg,
    /// Named external source roots, read through their own VCS.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, SourceRootCfg>,
}

/// Allowlist for `Command` ground sources (Claude-Code-style): only these
/// programs (`argv[0]`) may run. Empty = no command sources permitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommandCfg {
    #[serde(default)]
    pub allow: Vec<String>,
}

impl CommandCfg {
    pub fn allows(&self, program: &str) -> bool {
        self.allow.iter().any(|a| a == program)
    }
}

/// Which VCS a source root is read through, when a revision is pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vcs {
    #[default]
    Git,
    Jj,
}

/// Detect the VCS backing a directory (for pinned-revision reads). Defaults to
/// `Git` when neither a `.jj/` nor a `.git/` is present; a revision read then
/// fails with a clear VCS error rather than silently succeeding.
fn detect_vcs(root: &Path) -> Vcs {
    if root.join(".jj").is_dir() {
        Vcs::Jj
    } else {
        Vcs::Git
    }
}

/// A named source mount.
/// Either repo-backed (`repo`, read through its VCS) or handler-backed
/// (`handler`, a sandboxed Deno module that resolves a reference to bytes).
/// The two are mutually exclusive: a CURIE prefix is one mount, resolved one
/// way.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SourceRootCfg {
    /// Path to the repo, relative to the store root (e.g. `../wiki`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default)]
    pub vcs: Vcs,
    /// Path to a Deno handler module, relative to the store root (e.g.
    /// `handlers/wiki.ts`). Present iff this mount is handler-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
}

impl SourceRootCfg {
    /// The capabilities a handler-backed mount is entitled to, folded into the
    /// handler's content hash.
    pub fn caps(&self) -> Capabilities {
        Capabilities {
            net: self.net.clone(),
            read: self.read.clone(),
            env: self.env.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Daemon {
    /// Scheduler tick interval for `ken serve`.
    pub interval: FixedDuration,
}

impl Default for Daemon {
    fn default() -> Self {
        Daemon {
            interval: "60s".parse().expect("positive fixed interval"),
        }
    }
}

impl Daemon {
    pub fn interval_secs(&self) -> f64 {
        self.interval.as_secs_f64()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    /// Max verifier runs per scheduler tick.
    pub per_tick: usize,
    /// Base audit rate, scaled up by a fact's centrality.
    pub epsilon: f64,
    /// Max exploration-floor audits per tick, on top of `per_tick`.
    #[serde(default = "default_audit_per_tick")]
    pub audit_per_tick: usize,
    /// Max concurrent ground reads in a tick's read phase. `1` is serial.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

fn default_audit_per_tick() -> usize {
    5
}

fn default_concurrency() -> usize {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Decay {
    /// Applied at ingest when no half-life is supplied; saved with the fact.
    #[serde(default)]
    pub default_half_life: HalfLife,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sandbox {
    pub runtime: String,
    pub timeout: FixedDuration,
    #[serde(default)]
    pub default_caps: Vec<String>,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            per_tick: 20,
            epsilon: 0.02,
            audit_per_tick: default_audit_per_tick(),
            concurrency: default_concurrency(),
        }
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Sandbox {
            runtime: "deno".into(),
            timeout: "10s".parse().expect("positive fixed timeout"),
            default_caps: vec![],
        }
    }
}

impl Sandbox {
    pub fn timeout_secs(&self) -> f64 {
        self.timeout.as_secs_f64()
    }
}

impl Config {
    /// Load and parse a `ken.toml` from `path`.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or is not valid TOML.
    pub fn load(path: &std::path::Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    /// Resolve a class from an old fact without changing its configured decay.
    pub(crate) fn legacy_half_life(&self, class: &str) -> Result<HalfLife> {
        let default = match class {
            "immutable" => HalfLife::Never,
            "slow" => "P90D".parse().expect("positive default half-life"),
            "days" => HalfLife::default(),
            "hours" => "PT6H".parse().expect("positive default half-life"),
            _ => {
                return Err(Error::Config(format!(
                    "unknown legacy volatility class: {class}"
                )))
            }
        };
        Ok(self.volatility.get(class).copied().unwrap_or(default))
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Resolve a [`SourceRoot`] to a filesystem path and the VCS it is read
    /// through. `Project` is the store root's parent; `Store` is the store
    /// itself; `Named` comes from `[sources.*]`.
    ///
    /// # Errors
    /// Returns an error if the store has no parent project, or if a `Named`
    /// root is unknown or handler-backed rather than a repo.
    pub fn resolve_root(&self, root: &SourceRoot, store_root: &Path) -> Result<(PathBuf, Vcs)> {
        match root {
            SourceRoot::Project => {
                let parent = store_root
                    .parent()
                    .ok_or_else(|| Error::Config("store has no parent project".into()))?;
                let vcs = detect_vcs(parent);
                Ok((parent.to_path_buf(), vcs))
            }
            // The store is a plain directory now, not a repo: `store:` reads are
            // filesystem-only, so a pinned revision has no VCS to resolve it.
            SourceRoot::Store => Ok((store_root.to_path_buf(), Vcs::Git)),
            SourceRoot::Named(name) => {
                let cfg = self
                    .sources
                    .get(name)
                    .ok_or_else(|| Error::Config(format!("unknown source root `{name}`")))?;
                let repo = cfg.repo.as_ref().ok_or_else(|| {
                    Error::Config(format!(
                        "source root `{name}` is handler-backed, not a repo"
                    ))
                })?;
                Ok((store_root.join(repo), cfg.vcs))
            }
        }
    }

    /// The handler module path and capabilities for a handler-backed scheme, or
    /// `None` if the scheme is unknown or repo-backed.
    pub fn handler_for(&self, scheme: &str) -> Option<(String, Capabilities)> {
        let cfg = self.sources.get(scheme)?;
        let handler = cfg.handler.clone()?;
        Some((handler, cfg.caps()))
    }
}

/// Parse a positive elapsed duration, including ISO 8601 and shorthand.
/// Returns `None` for invalid input or calendar years and months.
pub fn parse_duration_secs(s: &str) -> Option<f64> {
    s.parse::<FixedDuration>()
        .ok()
        .map(FixedDuration::as_secs_f64)
}

/// Parse a positive elapsed duration.
///
/// # Errors
/// Returns an error for invalid input, or years and months without an anchor.
pub fn require_duration(s: &str) -> Result<f64> {
    s.parse::<FixedDuration>()
        .map(FixedDuration::as_secs_f64)
        .map_err(|e| Error::Config(format!("bad duration `{s}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_readme_example() {
        let toml = r#"
[budget]
per_tick = 20
epsilon  = 0.02

[decay]
default_half_life = "P3D"

[sandbox]
runtime      = "deno"
timeout      = "10s"
default_caps = []
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.budget.per_tick, 20);
        assert_eq!(cfg.budget.epsilon, 0.02);
        assert_eq!(cfg.decay.default_half_life, HalfLife::default());
        assert_eq!(cfg.sandbox.runtime, "deno");
        assert_eq!(cfg.sandbox.timeout_secs(), 10.0);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("never"), None);
        assert_eq!(parse_duration_secs("6h"), Some(21600.0));
        assert_eq!(parse_duration_secs("90d"), Some(7_776_000.0));
        assert_eq!(parse_duration_secs("30m"), Some(1800.0));
    }

    #[test]
    fn rejects_invalid_durations_in_every_config_field() {
        for input in [
            "[decay]\ndefault_half_life = \"0s\"",
            "[decay]\ndefault_half_life = \"oops\"",
            "[decay]\ndefault_half_life = \"-P1D\"",
            "[daemon]\ninterval = \"never\"",
            "[daemon]\ninterval = \"P1M\"",
            "[daemon]\ninterval = \"PT0S\"",
            "[sandbox]\nruntime = \"deno\"\ntimeout = \"NaN\"",
            "[volatility]\nhours = \"typo\"",
        ] {
            assert!(toml::from_str::<Config>(input).is_err(), "{input}");
        }
    }

    #[test]
    fn duration_forms_roundtrip_through_config() {
        let cfg: Config = toml::from_str(
            r#"
[decay]
default_half_life = "P1M"
[daemon]
interval = "1 minute, 30 seconds"
[sandbox]
runtime = "deno"
timeout = "PT0.5S"
"#,
        )
        .unwrap();
        assert_eq!(cfg.decay.default_half_life.to_string(), "P1M");
        assert_eq!(cfg.daemon.interval_secs(), 90.0);
        assert_eq!(cfg.sandbox.timeout_secs(), 0.5);
        assert_eq!(toml::from_str::<Config>(&cfg.to_toml()).unwrap(), cfg);
    }

    #[test]
    fn default_roundtrips() {
        let cfg = Config::default();
        let s = cfg.to_toml();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }
}
