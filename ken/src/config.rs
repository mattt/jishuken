//! `ken.toml` parsing (README "Configuration"). Lives in the store root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::{Capabilities, SourceRoot, Volatility};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub volatility: VolatilityMap,
    #[serde(default)]
    pub sandbox: Sandbox,
    #[serde(default)]
    pub daemon: Daemon,
    #[serde(default)]
    pub command: CommandCfg,
    /// Named external source roots, read through their own VCS (README
    /// "Configuration": `[sources.wiki]`).
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

/// Which VCS a source root is read through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vcs {
    #[default]
    Jj,
    Git,
}

/// A named source mount (README "Configuration"). Either repo-backed (`repo`,
/// read through its VCS) or handler-backed (`handler`, a sandboxed Deno module
/// that resolves a reference to bytes). The two are mutually exclusive: a CURIE
/// prefix is one mount, resolved one way.
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
    pub interval: String,
}

impl Default for Daemon {
    fn default() -> Self {
        Daemon {
            interval: "60s".into(),
        }
    }
}

impl Daemon {
    pub fn interval_secs(&self) -> f64 {
        parse_duration_secs(&self.interval).unwrap_or(60.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    /// Max verifier runs per scheduler tick.
    pub per_tick: usize,
    /// Base audit rate, scaled up by a fact's centrality.
    pub epsilon: f64,
    /// Max exploration-floor audits per tick (DESIGN §7), on top of `per_tick`.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolatilityMap {
    pub immutable: String,
    pub slow: String,
    pub days: String,
    pub hours: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sandbox {
    pub runtime: String,
    pub timeout: String,
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

impl Default for VolatilityMap {
    fn default() -> Self {
        VolatilityMap {
            immutable: "never".into(),
            slow: "90d".into(),
            days: "3d".into(),
            hours: "6h".into(),
        }
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Sandbox {
            runtime: "deno".into(),
            timeout: "10s".into(),
            default_caps: vec![],
        }
    }
}

impl VolatilityMap {
    /// Half-life in seconds for a class; `None` means it never decays.
    pub fn half_life_secs(&self, v: Volatility) -> Option<f64> {
        let raw = match v {
            Volatility::Immutable => &self.immutable,
            Volatility::Slow => &self.slow,
            Volatility::Days => &self.days,
            Volatility::Hours => &self.hours,
        };
        parse_duration_secs(raw)
    }
}

impl Sandbox {
    pub fn timeout_secs(&self) -> f64 {
        parse_duration_secs(&self.timeout).unwrap_or(10.0)
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

    pub fn load_or_default(path: &std::path::Path) -> Config {
        Config::load(path).unwrap_or_default()
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
                Ok((parent.to_path_buf(), Vcs::Jj))
            }
            SourceRoot::Store => Ok((store_root.to_path_buf(), Vcs::Jj)),
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

/// Parse a duration like `90d`, `6h`, `30m`, `10s`. `"never"`/`"immutable"` →
/// `None` (no decay).
pub fn parse_duration_secs(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("never") || s.eq_ignore_ascii_case("immutable") {
        return None;
    }
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "s" | "" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        "w" => 604_800.0,
        _ => return None,
    };
    Some(n * mult)
}

/// Validate that an op constructor was not asked to set an unknown duration.
///
/// # Errors
/// Returns an error if `s` is not a recognized duration like `90d` or `6h`.
pub fn require_duration(s: &str) -> Result<f64> {
    parse_duration_secs(s).ok_or_else(|| Error::Config(format!("bad duration: {s}")))
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

[volatility]
immutable = "never"
slow      = "90d"
days      = "3d"
hours     = "6h"

[sandbox]
runtime      = "deno"
timeout      = "10s"
default_caps = []
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.budget.per_tick, 20);
        assert_eq!(cfg.budget.epsilon, 0.02);
        assert_eq!(cfg.volatility.half_life_secs(Volatility::Immutable), None);
        assert_eq!(
            cfg.volatility.half_life_secs(Volatility::Hours),
            Some(6.0 * 3600.0)
        );
        assert_eq!(
            cfg.volatility.half_life_secs(Volatility::Days),
            Some(3.0 * 86400.0)
        );
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
    fn default_roundtrips() {
        let cfg = Config::default();
        let s = cfg.to_toml();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }
}
