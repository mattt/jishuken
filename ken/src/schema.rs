//! Fact schema.
//! Normalized join keys, denormalized payload.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type Timestamp = DateTime<Utc>;
pub type Symbol = String;

/// Stable fact identity. The normalized claim (`entity.relation`) *is* the
/// identity: it is already canonical and unique, and it survives every
/// re-verification because a verifier run changes the value, never the key. A
/// key change is a different claim, i.e. a different fact, so no opaque id is
/// needed (and hashing a unique key to "look like" an id would be pure
/// ceremony).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FactId(pub String);

impl FactId {
    /// The fact's identity: its `entity.relation` key, verbatim and greppable.
    pub fn for_claim(claim: &Claim) -> Self {
        FactId(claim.key())
    }
}

impl std::fmt::Display for FactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Canonical node identity.
/// Never a string recurring in many blobs; the scheduler traverses these.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId(pub String);

/// The join keys. Normalized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub entity: EntityId,
    pub relation: Symbol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nl: Option<String>,
}

impl Claim {
    /// Parse the `entity.relation` key form used across the CLI/MCP surface.
    ///
    /// # Errors
    /// Returns [`KeyError::Missing`] if `key` is not of the form
    /// `entity.relation`.
    pub fn parse_key(key: &str) -> Result<Claim, KeyError> {
        let (entity, relation) = key.rsplit_once('.').ok_or(KeyError::Missing)?;
        if entity.is_empty() || relation.is_empty() {
            return Err(KeyError::Missing);
        }
        Ok(Claim {
            entity: EntityId(entity.to_string()),
            relation: relation.to_string(),
            nl: None,
        })
    }

    pub fn key(&self) -> String {
        format!("{}.{}", self.entity.0, self.relation)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("key must be of the form `entity.relation`")]
    Missing,
}

/// The payload. Denormalized; only the verifier and the LLM read inside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FactValue {
    Scalar { value: serde_json::Value },
    Set { elements: Vec<Element> },
}

impl FactValue {
    /// The value as it is passed to a verifier via `KEN_VALUE`.
    pub fn as_env(&self) -> String {
        match self {
            FactValue::Scalar { value } => match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            FactValue::Set { elements } => {
                serde_json::to_string(&elements.iter().map(|e| &e.value).collect::<Vec<_>>())
                    .unwrap_or_default()
            }
        }
    }

    pub fn render(&self) -> String {
        match self {
            FactValue::Scalar { value } => match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            FactValue::Set { elements } => {
                let items: Vec<String> = elements.iter().map(|e| e.value.to_string()).collect();
                format!("[{}]", items.join(", "))
            }
        }
    }
}

/// An assertion: the atomic proposition.
/// Carries its own epistemics only when it earns them, so gain and conflict
/// land per element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub value: serde_json::Value,
    pub seen: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<ConflictMarker>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConflictMarker {
    pub verifiers: Vec<GeneratorHash>,
}

/// Confidence and groundedness must never merge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Epistemics {
    pub confidence: f64,
    pub groundedness: Groundedness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Groundedness {
    /// An ingested or manually doubted claim.
    Ungrounded { source: TriageSource },
    /// The checked grounds confirm the claim, with no refutations.
    Verified { at: Timestamp, by: GeneratorHash },
    /// The checked grounds refute the claim, with no confirmations.
    Refuted { at: Timestamp, by: GeneratorHash },
    /// Some checked grounds confirm the claim and others refute it.
    Conflicted { verifiers: Vec<GeneratorHash> },
}

impl Groundedness {
    /// Sybil-resistant centrality weight: ungrounded and refuted contribute zero,
    /// conflicted contributes partial, verified contributes full.
    pub fn trust_weight(&self) -> f64 {
        match self {
            Groundedness::Ungrounded { .. } | Groundedness::Refuted { .. } => 0.0,
            Groundedness::Conflicted { .. } => 0.5,
            Groundedness::Verified { .. } => 1.0,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Groundedness::Ungrounded { .. } => "ungrounded",
            Groundedness::Verified { .. } => "verified",
            Groundedness::Refuted { .. } => "refuted",
            Groundedness::Conflicted { .. } => "conflicted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "lowercase")]
pub enum TriageSource {
    Llm { meta_confidence: f64 },
    Ingest,
    Manual,
}

/// Volatility class sets the process noise Q.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Volatility {
    Immutable,
    Slow,
    Days,
    Hours,
}

impl std::str::FromStr for Volatility {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "immutable" => Ok(Volatility::Immutable),
            "slow" => Ok(Volatility::Slow),
            "days" => Ok(Volatility::Days),
            "hours" => Ok(Volatility::Hours),
            other => Err(format!("unknown volatility class: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleMeta {
    pub volatility: Volatility,
    pub centrality: f64,
    pub last_verified: Timestamp,
    /// Posterior variance at the last verification; inflates with `q * dt`.
    pub variance_at_verify: f64,
    /// Scheduler-set ordering priority (never truth).
    pub priority: f64,
}

/// Content hash of a source generator (covers source AND capabilities), or a
/// synthetic marker (`file`, `command`, `existence`) for non-generator sources.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GeneratorHash(pub String);

impl std::fmt::Display for GeneratorHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A content-addressed source generator: sandboxed code that emits the ground
/// value.
/// The capability set lives inside the hash, so a generator that quietly starts
/// asking for the network produces a loud diff and a grant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratorRef {
    pub hash: GeneratorHash,
    pub caps: Capabilities,
    pub cost_estimate: f64,
    /// On-disk source path, relative to the store root.
    pub src_path: String,
    /// Human label, e.g. `notion-roster.ts`.
    pub name: String,
}

impl GeneratorRef {
    /// `caps.net` non-empty makes a generator the expensive, less-trusted tier.
    pub fn is_net(&self) -> bool {
        !self.caps.net.is_empty()
    }

    /// `name@hashprefix`, the form shown in `recall`/`why`.
    pub fn display(&self) -> String {
        let short = &self.hash.0[..self.hash.0.len().min(6)];
        format!("{}@{}", self.name, short)
    }
}

/// A content-addressed scheme handler: sandboxed code mounted under a CURIE
/// prefix (`[sources.<scheme>]` with a `handler`) that resolves a reference to
/// document bytes.
/// Like a generator, its capability set lives inside the hash, so a handler
/// that starts asking for the network produces a loud diff and a re-ground.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandlerRef {
    pub hash: GeneratorHash,
    pub caps: Capabilities,
    pub cost_estimate: f64,
    /// On-disk source path, relative to the store root (e.g. `handlers/wiki.ts`).
    pub src_path: String,
    /// The CURIE scheme this handler is mounted under (e.g. `wiki`).
    pub scheme: String,
}

impl HandlerRef {
    /// `caps.net` non-empty makes a handler the expensive, less-trusted tier.
    pub fn is_net(&self) -> bool {
        !self.caps.net.is_empty()
    }

    /// `scheme@hashprefix`, the form shown in `recall`/`why`.
    pub fn display(&self) -> String {
        let short = &self.hash.0[..self.hash.0.len().min(6)];
        format!("{}@{}", self.scheme, short)
    }
}

/// A scheme-handler read: the mounted handler plus the reference (the CURIE's
/// path, e.g. `Architecture`) and an optional pinned revision handed to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandlerSource {
    pub handler: HandlerRef,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub net: Vec<String>,
    #[serde(default)]
    pub read: Vec<String>,
    /// Host environment variables the sandbox may read (e.g. an auth token a
    /// scheme handler needs). Folded into the content hash like `net`/`read`.
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub ingested_by: String,
    pub ingested_at: Timestamp,
}

/// Result of a ground check against a ground source.
/// Lives here because both the write path and a ground binding's recorded state
/// need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Confirmed,
    Refuted,
    /// The source could not be read in a way we can trust (spawn/exec failure,
    /// allowlist denial, generator crash). Not a refutation.
    Errored,
    /// A transient read failure on a non-deterministic channel (net timeout,
    /// 5xx). Schedules a retry; does not move belief.
    Inconclusive,
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Confirmed => "confirmed",
            Outcome::Refuted => "refuted",
            Outcome::Errored => "errored",
            Outcome::Inconclusive => "inconclusive",
        }
    }

    /// Only confirmed/refuted move confidence and feed calibration.
    pub fn updates_belief(&self) -> bool {
        matches!(self, Outcome::Confirmed | Outcome::Refuted)
    }
}

/// Where a source root resolves.
/// `Named` roots come from `[sources.*]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceRoot {
    /// The project the store lives inside (the store root's parent).
    Project,
    /// The ken store itself.
    Store,
    /// A configured external repo, resolved via `[sources.<name>]`.
    Named(String),
}

/// A typed reference to where a truth lives: a root, a path within it, and an
/// optional pinned revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub root: SourceRoot,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

/// A host command run to produce a ground value (HTTP folds in here as `curl`).
/// Gated by the `[command]` allowlist on `argv[0]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSource {
    pub argv: Vec<String>,
    /// Working directory root for the command.
    #[serde(default = "project_root")]
    pub root: SourceRoot,
}

fn project_root() -> SourceRoot {
    SourceRoot::Project
}

/// The kind of ground source. `File` reads bytes; `Command` runs an allowlisted
/// host program; `Generator` runs sandboxed code that emits the value. Code and
/// capabilities live here (the *reading* layer), never in the judge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum GroundSource {
    File(SourceRef),
    Command(CommandSource),
    Generator(GeneratorRef),
    /// A scheme handler mounted under a CURIE prefix: code that resolves a
    /// reference to bytes, which the locator then projects.
    Handler(HandlerSource),
}

/// Channel trust weights: how much a confirmation read over each channel
/// counts.
/// A local file read is deterministic and un-eclipse-able (full weight); a
/// command or net-capable generator is attacker-influenceable, so its gain is
/// discounted.
const WEIGHT_FILE: f64 = 1.0;
const WEIGHT_COMMAND: f64 = 0.7;
const WEIGHT_NET: f64 = 0.5;
const WEIGHT_LOCAL_GENERATOR: f64 = 0.9;

/// Per-read `VoI` costs: a file read is cheap; a command spawns a host process.
/// Generators and handlers carry their own `cost_estimate` instead.
const COST_FILE: f64 = 1.0;
const COST_COMMAND: f64 = 5.0;

/// The channel weight for a generator/handler, by whether it holds `net`.
fn weight_for_net(is_net: bool) -> f64 {
    if is_net {
        WEIGHT_NET
    } else {
        WEIGHT_LOCAL_GENERATOR
    }
}

impl GroundSource {
    /// Trust weight of the channel: a local file read is deterministic and
    /// un-eclipse-able, so it counts full; a command or a net-capable generator
    /// is more eclipse-able, so it discounts the gain.
    pub fn channel_weight(&self) -> f64 {
        match self {
            GroundSource::File(_) => WEIGHT_FILE,
            GroundSource::Command(_) => WEIGHT_COMMAND,
            GroundSource::Generator(g) => weight_for_net(g.is_net()),
            GroundSource::Handler(h) => weight_for_net(h.handler.is_net()),
        }
    }

    /// Whether reading this source goes over a non-deterministic channel
    /// (net-capable generator). Commands are treated as potentially networked.
    pub fn is_net(&self) -> bool {
        match self {
            GroundSource::File(_) => false,
            GroundSource::Command(_) => true,
            GroundSource::Generator(g) => g.is_net(),
            GroundSource::Handler(h) => h.handler.is_net(),
        }
    }

    /// Per-read cost for `VoI`: a file read is cheap; a command or generator
    /// spawns a process.
    pub fn cost(&self) -> f64 {
        match self {
            GroundSource::File(_) => COST_FILE,
            GroundSource::Command(_) => COST_COMMAND,
            GroundSource::Generator(g) => g.cost_estimate,
            GroundSource::Handler(h) => h.handler.cost_estimate,
        }
    }

    pub fn kind_label(&self) -> String {
        match self {
            GroundSource::File(_) => "FILE".to_string(),
            GroundSource::Command(_) => "CMD".to_string(),
            GroundSource::Generator(_) => "GEN".to_string(),
            // The mount's scheme, uppercased (e.g.
            // `WIKI`).
            GroundSource::Handler(h) => h.handler.scheme.to_uppercase(),
        }
    }
}

/// Points at the span within a source the fact is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "loc", rename_all = "lowercase")]
pub enum Locator {
    /// `Doc.md#heading` — a Markdown section.
    Heading { heading: String },
    /// `Doc.md?q="..."` — a quoted substring, re-found if it drifts.
    Quote { needle: String },
    /// `file#L40-58` — a line range, always paired with the span hash.
    LineRange { start: usize, end: usize },
    /// `file#ts:(query)` — a tree-sitter match. Parsed but resolution deferred.
    TreeSitter { lang: String, query: String },
    /// The whole file.
    Whole,
}

/// The state a ground check last resolved to: the replay binding.
/// A confirmation is bound to the exact span it saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub rev: String,
    pub span_hash: String,
    pub at: Timestamp,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<GeneratorHash>,
}

/// One independent grounding of a fact: a typed source (where/how to read), a
/// locator (which span), and a predicate (how to judge). The source carries any
/// code and capabilities; the predicate is pure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroundBinding {
    pub source: GroundSource,
    pub locator: Locator,
    #[serde(default)]
    pub predicate: crate::predicate::Predicate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Resolved>,
}

impl GroundBinding {
    /// A read over a non-deterministic channel; its gain is discounted.
    pub fn is_net(&self) -> bool {
        self.source.is_net()
    }

    /// Per-check cost for `VoI`: a file read is cheap, a command/generator spawns.
    pub fn cost(&self) -> f64 {
        self.source.cost()
    }
}

/// Believed. Needs verification. The addressable, scheduled, verified unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub id: FactId,
    pub claim: Claim,
    pub value: FactValue,
    pub epistemics: Epistemics,
    pub schedule: ScheduleMeta,
    /// Independent groundings of the same proposition.
    /// Empty until the control plane binds one.
    #[serde(default)]
    pub grounds: Vec<GroundBinding>,
    pub provenance: Provenance,
}

impl Fact {
    /// Deterministic on-disk path: `facts/<entity>/<relation>.json`.
    pub fn rel_path(&self) -> String {
        path_for(&self.claim)
    }

    /// A fact is checkable once it has at least one grounding.
    pub fn is_checkable(&self) -> bool {
        !self.grounds.is_empty()
    }

    /// Cheapest ground check, for `VoI` ranking. `None` if unchecked-able.
    pub fn min_check_cost(&self) -> Option<f64> {
        self.grounds
            .iter()
            .map(GroundBinding::cost)
            .min_by(f64::total_cmp)
    }
}

pub fn path_for(claim: &Claim) -> String {
    format!(
        "facts/{}/{}.json",
        sanitize(&claim.entity.0),
        sanitize(&claim.relation)
    )
}

/// Keep keys filesystem-safe while staying greppable.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrips() {
        let c = Claim::parse_key("staging.url").unwrap();
        assert_eq!(c.entity.0, "staging");
        assert_eq!(c.relation, "url");
        assert_eq!(c.key(), "staging.url");
    }

    #[test]
    fn dotted_entity_takes_last_segment_as_relation() {
        let c = Claim::parse_key("product.eu.markets").unwrap();
        assert_eq!(c.entity.0, "product.eu");
        assert_eq!(c.relation, "markets");
    }

    #[test]
    fn bad_keys_rejected() {
        assert!(Claim::parse_key("nodot").is_err());
        assert!(Claim::parse_key(".").is_err());
    }

    #[test]
    fn fact_id_is_the_key_verbatim() {
        let c = Claim::parse_key("staging.url").unwrap();
        assert_eq!(FactId::for_claim(&c), FactId::for_claim(&c));
        // Identity is the key itself: no hash, no opaque id.
        assert_eq!(FactId::for_claim(&c).0, "staging.url");
    }

    #[test]
    fn trust_weight_ranks_groundedness() {
        let ts = Utc::now();
        let u = Groundedness::Ungrounded {
            source: TriageSource::Ingest,
        };
        let v = Groundedness::Verified {
            at: ts,
            by: GeneratorHash("x".into()),
        };
        let c = Groundedness::Conflicted { verifiers: vec![] };
        let r = Groundedness::Refuted {
            at: ts,
            by: GeneratorHash("x".into()),
        };
        assert_eq!(u.trust_weight(), 0.0);
        assert_eq!(r.trust_weight(), 0.0);
        assert_eq!(c.trust_weight(), 0.5);
        assert_eq!(v.trust_weight(), 1.0);
    }
}
