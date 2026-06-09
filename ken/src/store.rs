//! The storage seam (DESIGN §4). CLI-first: the [`VersionedStore`] trait sits at
//! the boundary and [`JjStore`] implements it over the `jj` CLI. A fact is a
//! file at `facts/<entity>/<relation>.json`, so content-addressing is free and a
//! concurrent disagreeing write becomes a real jj conflict instead of a lost
//! update. Every [`WriteOp`] becomes one tagged jj operation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::Utc;

use crate::calibration::CalibrationSample;
use crate::centrality::FactGraph;
use crate::config::Config;
use crate::decay::{decayed_variance, kalman_update};
use crate::error::{Error, Result};
use crate::ground::generator::GeneratorRegistry;
use crate::schema::{
    path_for, ChangeId, Claim, Epistemics, Fact, FactValue, GeneratorHash, GroundBinding,
    Groundedness, Provenance, ScheduleMeta, Timestamp,
};
use crate::sketch::TDigest;
use crate::write::{landed_confidence_with, Outcome, WriteOp};

/// Which revision to read at. `@` is the working copy.
#[derive(Debug, Clone)]
pub enum Rev {
    Working,
    At(String),
}

impl Rev {
    fn as_arg(&self) -> String {
        match self {
            Rev::Working => "@".to_string(),
            Rev::At(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub id: String,
    pub time: String,
    pub description: String,
}

/// Handle to an anonymous hypothesis change (DESIGN §4).
#[derive(Debug, Clone)]
pub struct Workspace {
    pub change: String,
}

/// The store, reached through one trait (README "Library"). Every method
/// returns an error if the underlying store cannot be read or written, or (for
/// historical revisions) the `jj` invocation fails.
pub trait VersionedStore {
    /// Read a fact by id at a given revision.
    ///
    /// # Errors
    /// See the trait-level note.
    fn read_fact(&self, id: &ChangeId, at: Rev) -> Result<Fact>;
    /// Apply one [`WriteOp`], landing it as one tagged jj operation.
    ///
    /// # Errors
    /// See the trait-level note.
    fn apply(&self, op: WriteOp) -> Result<ChangeId>;
    /// The facts currently in a `Conflicted` state.
    ///
    /// # Errors
    /// See the trait-level note.
    fn list_conflicts(&self) -> Result<Vec<ChangeId>>;
    /// The operation log, newest first, optionally truncated at `since`.
    ///
    /// # Errors
    /// See the trait-level note.
    fn op_log(&self, since: Option<OpId>) -> Result<Vec<Operation>>;
    /// Start an anonymous hypothesis change off `base`.
    ///
    /// # Errors
    /// See the trait-level note.
    fn branch_hypothesis(&self, base: Rev) -> Result<Workspace>;
}

/// Initial posterior variance for a freshly ingested, unverified fact: high, so
/// the first verifier run moves it a lot.
const INGEST_VARIANCE: f64 = 0.25;

/// ASCII separators used to parse the jj op-log template unambiguously.
const US: char = '\u{1f}';
const RS: char = '\u{1e}';

#[derive(Debug)]
pub struct JjStore {
    root: PathBuf,
    config: Config,
}

impl JjStore {
    /// Discover the store by walking up from `start` for a `.ken/` directory,
    /// the way jj finds `.jj/`. Honors an explicit override first.
    ///
    /// # Errors
    /// Returns [`Error::StoreNotFound`] if no `.ken/` store is found.
    pub fn discover(explicit: Option<&Path>, start: &Path) -> Result<JjStore> {
        if let Some(p) = explicit {
            return JjStore::open(p);
        }
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(".ken");
            if candidate.join(".jj").is_dir() {
                return JjStore::open(&candidate);
            }
            dir = d.parent();
        }
        Err(Error::StoreNotFound)
    }

    /// Open an existing `.ken/` store at `root`.
    ///
    /// # Errors
    /// Returns [`Error::StoreNotFound`] if `root` is not a jj-backed store, or
    /// [`Error::Jj`] if the installed `jj` is missing or older than
    /// [`JJ_MIN_VERSION`].
    pub fn open(root: &Path) -> Result<JjStore> {
        ensure_jj_supported()?;
        if !root.join(".jj").is_dir() {
            return Err(Error::StoreNotFound);
        }
        let config = Config::load_or_default(&root.join("ken.toml"));
        Ok(JjStore {
            root: root.to_path_buf(),
            config,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn registry(&self) -> GeneratorRegistry {
        GeneratorRegistry::new(&self.root)
    }

    /// Create a `.ken/` store: a jj repo with `ken.toml`, `facts/`, `verifiers/`.
    ///
    /// # Errors
    /// Returns an error if the directories cannot be created, a `jj` invocation
    /// fails, or the installed `jj` is missing or older than [`JJ_MIN_VERSION`].
    pub fn init(root: &Path) -> Result<JjStore> {
        ensure_jj_supported()?;
        std::fs::create_dir_all(root)?;
        // `git init` must not pass `-R` (no repo exists yet to resolve).
        run_jj_raw(root, &["git", "init", "."])?;
        // Pin an identity so commits never block on missing user config.
        run_jj_in(root, &["config", "set", "--repo", "user.name", "ken"])?;
        run_jj_in(
            root,
            &["config", "set", "--repo", "user.email", "ken@localhost"],
        )?;
        std::fs::create_dir_all(root.join("facts"))?;
        std::fs::create_dir_all(root.join("verifiers"))?;
        let cfg = Config::default();
        std::fs::write(root.join("ken.toml"), cfg.to_toml())?;
        run_jj_in(root, &["describe", "-m", "[Init] ken store"])?;
        run_jj_in(root, &["new"])?;
        JjStore::open(root)
    }

    fn jj(&self, args: &[&str]) -> Result<String> {
        run_jj_in(&self.root, args)
    }

    fn fact_path(&self, claim: &Claim) -> PathBuf {
        self.root.join(path_for(claim))
    }

    /// Read a fact by its `entity.relation` key from the working copy.
    ///
    /// # Errors
    /// Returns an error if the key is malformed, the fact does not exist, or its
    /// file is not valid JSON.
    pub fn read_fact_by_key(&self, key: &str) -> Result<Fact> {
        let claim = Claim::parse_key(key)?;
        let path = self.fact_path(&claim);
        let bytes = std::fs::read(&path).map_err(|_| Error::FactNotFound(key.to_string()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// All facts currently in the store (scans `facts/`).
    ///
    /// # Errors
    /// Returns an error if the `facts/` directory cannot be read.
    pub fn all_facts(&self) -> Result<Vec<Fact>> {
        let mut out = Vec::new();
        let facts_dir = self.root.join("facts");
        if !facts_dir.is_dir() {
            return Ok(out);
        }
        for entity in std::fs::read_dir(&facts_dir)? {
            let entity = entity?;
            if !entity.path().is_dir() {
                continue;
            }
            for rel in std::fs::read_dir(entity.path())? {
                let rel = rel?;
                if rel.path().extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(rel.path()) {
                    if let Ok(f) = serde_json::from_slice::<Fact>(&bytes) {
                        out.push(f);
                    }
                }
            }
        }
        Ok(out)
    }

    fn write_fact(&self, fact: &Fact) -> Result<()> {
        let path = self.fact_path(&fact.claim);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(fact)?)?;
        Ok(())
    }

    /// Persist a fact file without committing. Control-plane callers use this
    /// alongside [`JjStore::control_commit`] to land one tagged op.
    ///
    /// # Errors
    /// Returns an error if the fact file cannot be written.
    pub fn put_fact(&self, fact: &Fact) -> Result<()> {
        self.write_fact(fact)
    }

    /// Commit the working copy as one tagged jj operation (DESIGN §3, §4).
    fn commit(&self, tag: &str, detail: &str) -> Result<()> {
        let msg = format!("[{tag}] {detail}");
        self.jj(&["commit", "-m", &msg])?;
        Ok(())
    }

    /// Control-plane tagged commit (e.g. `Grant`), one loud logged operation.
    ///
    /// # Errors
    /// Returns an error if the `jj commit` invocation fails.
    pub fn control_commit(&self, tag: &str, detail: &str) -> Result<()> {
        self.commit(tag, detail)
    }

    fn current_change_id(&self) -> Result<String> {
        let out = self.jj(&["log", "-r", "@", "--no-graph", "-T", "change_id.short()"])?;
        Ok(out.trim().to_string())
    }

    fn append_calibration(&self, sample: &CalibrationSample) -> Result<()> {
        use std::io::Write;
        let path = self.root.join("calibration.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{}", serde_json::to_string(sample)?)?;
        Ok(())
    }

    /// The logged calibration samples, or an empty list if none exist yet.
    ///
    /// # Errors
    /// Currently infallible; returns `Result` for symmetry with the other
    /// store readers.
    pub fn calibration_samples(&self) -> Result<Vec<CalibrationSample>> {
        let path = self.root.join("calibration.jsonl");
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(vec![]);
        };
        Ok(text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    fn costs_path(&self) -> PathBuf {
        self.root.join("costs.json")
    }

    /// Per-generator t-digests of observed run times, keyed by generator hash.
    fn load_costs(&self) -> HashMap<String, TDigest> {
        match std::fs::read(self.costs_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => HashMap::new(),
        }
    }

    /// Fold one observed run time (seconds) into a generator's cost sketch
    /// (Idea 2). Keyed by the content hash, so the measurement pools across
    /// every fact that shares the generator and merges across revisions.
    ///
    /// # Errors
    /// Returns an error if the cost sketch file cannot be written.
    pub fn record_cost(&self, by: &GeneratorHash, secs: f64) -> Result<()> {
        let mut costs = self.load_costs();
        costs.entry(by.0.clone()).or_default().insert(secs);
        std::fs::write(self.costs_path(), serde_json::to_vec_pretty(&costs)?)?;
        Ok(())
    }

    /// Materialize the median observed run time per generator, for the `VoI`
    /// cost denominator. Generators with no samples are absent (the caller falls
    /// back to the static `cost_estimate`).
    pub fn cost_table(&self) -> HashMap<GeneratorHash, f64> {
        self.load_costs()
            .into_iter()
            .filter_map(|(hash, digest)| digest.quantile(0.5).map(|p50| (GeneratorHash(hash), p50)))
            .collect()
    }

    /// Roll the store back one operation (`ken undo`).
    ///
    /// # Errors
    /// Returns an error if the `jj undo` invocation fails.
    pub fn undo(&self) -> Result<()> {
        self.jj(&["undo"]).map(|_| ())
    }

    /// The tagged change descriptions (`[Ingest] …`, `[GroundCheck:confirmed] …`),
    /// newest first. This is where each [`WriteOp`]'s variant is recorded, so the
    /// audit "groundedness only ever moved via `GroundCheck`" is greppable here.
    ///
    /// # Errors
    /// Returns an error if the `jj log` invocation fails.
    pub fn change_log(&self) -> Result<Vec<Operation>> {
        let template = format!(
            "change_id.short() ++ \"{US}\" ++ committer.timestamp() ++ \"{US}\" ++ description ++ \"{RS}\""
        );
        let out = self.jj(&["log", "-r", "::@", "--no-graph", "-T", &template])?;
        Ok(parse_log_records(&out)
            .into_iter()
            .filter(|o| !o.description.is_empty())
            .collect())
    }

    /// Recompute trust-weighted centrality (DESIGN §9) over a fact→entity graph:
    /// an entity is central in proportion to the *grounded* facts that touch it,
    /// and each fact inherits the centrality of its entity. Ungrounded facts
    /// contribute zero, so data-plane injection cannot inflate the budget.
    ///
    /// Returns `true` if any fact's centrality changed (and was rewritten), so
    /// callers that coalesce the recompute can skip an empty commit.
    ///
    /// # Errors
    /// Returns an error if the facts cannot be read or rewritten.
    pub fn recompute_centrality(&self) -> Result<bool> {
        let mut facts = self.all_facts()?;
        if facts.is_empty() {
            return Ok(false);
        }
        let mut g = FactGraph::new();
        for f in &facts {
            g.add_node(f.id.0.clone(), f.epistemics.groundedness.trust_weight());
            g.add_node(format!("entity:{}", f.claim.entity.0), 1.0);
        }
        for f in &facts {
            g.add_edge(&f.id.0, &format!("entity:{}", f.claim.entity.0));
        }
        let c = g.katz_trust_weighted(0.5, 50);
        let mut changed = false;
        for f in &mut facts {
            let centrality = c
                .get(&format!("entity:{}", f.claim.entity.0))
                .copied()
                .unwrap_or(1.0);
            if (f.schedule.centrality - centrality).abs() > 1e-9 {
                f.schedule.centrality = centrality;
                self.write_fact(f)?;
                changed = true;
            }
        }
        Ok(changed)
    }
}

impl VersionedStore for JjStore {
    fn read_fact(&self, id: &ChangeId, at: Rev) -> Result<Fact> {
        match at {
            Rev::Working => {
                for f in self.all_facts()? {
                    if &f.id == id {
                        return Ok(f);
                    }
                }
                Err(Error::FactNotFound(id.0.clone()))
            }
            Rev::At(_) => {
                // At a historical rev, locate the fact's path then `jj file show`.
                let fact = self.read_fact(id, Rev::Working)?;
                let path = path_for(&fact.claim);
                let text = self.jj(&["file", "show", "-r", &at.as_arg(), &path])?;
                Ok(serde_json::from_str(&text)?)
            }
        }
    }

    fn apply(&self, op: WriteOp) -> Result<ChangeId> {
        self.apply_op(op, true)
    }

    fn list_conflicts(&self) -> Result<Vec<ChangeId>> {
        Ok(self
            .all_facts()?
            .into_iter()
            .filter(|f| matches!(f.epistemics.groundedness, Groundedness::Conflicted { .. }))
            .map(|f| f.id)
            .collect())
    }

    fn op_log(&self, since: Option<OpId>) -> Result<Vec<Operation>> {
        // jj templates don't accept `\u{..}` escapes, so embed the ASCII unit/
        // record separators (US 0x1f, RS 0x1e) directly inside the strings.
        let template = format!(
            "self.id().short() ++ \"{US}\" ++ self.time().start() ++ \"{US}\" ++ self.description() ++ \"{RS}\""
        );
        let out = self.jj(&["op", "log", "--no-graph", "-T", &template])?;
        let mut ops = Vec::new();
        for op in parse_log_records(&out) {
            if let Some(OpId(stop)) = &since {
                if &op.id == stop {
                    break;
                }
            }
            ops.push(op);
        }
        Ok(ops)
    }

    fn branch_hypothesis(&self, base: Rev) -> Result<Workspace> {
        self.jj(&["new", &base.as_arg()])?;
        Ok(Workspace {
            change: self.current_change_id()?,
        })
    }
}

impl JjStore {
    /// Apply a write op without recomputing centrality. The scheduler tick
    /// (Idea 1) uses this to coalesce N per-check recomputes into one after the
    /// loop; `recompute_centrality` is then called once and committed separately.
    ///
    /// # Errors
    /// Returns an error if the underlying write or `jj` invocation fails.
    pub fn apply_no_centrality(&self, op: WriteOp) -> Result<ChangeId> {
        self.apply_op(op, false)
    }

    fn apply_op(&self, op: WriteOp, recompute_centrality: bool) -> Result<ChangeId> {
        let tag = op.tag();
        match op {
            WriteOp::Ingest {
                claim,
                value,
                triage,
                volatility,
                draft_ground,
            } => {
                let now = Utc::now();
                let id = ChangeId::for_claim(&claim);
                // DESIGN §8: grade the LLM triage that produced this prior. The
                // Platt map fitted from logged (prior, outcome) pairs is applied
                // to LLM-triaged confidence; identity until enough samples.
                let recalibrator = crate::calibration::recalibrate(&self.calibration_samples()?);
                let fact = Fact {
                    id: id.clone(),
                    claim: claim.clone(),
                    value,
                    epistemics: Epistemics {
                        confidence: landed_confidence_with(&triage, &recalibrator),
                        groundedness: Groundedness::Ungrounded { source: triage },
                    },
                    schedule: ScheduleMeta {
                        volatility,
                        centrality: 1.0,
                        last_verified: now,
                        variance_at_verify: INGEST_VARIANCE,
                        priority: 0.0,
                    },
                    // A `--ground` hint lands as a draft binding (no verifier,
                    // unchecked); it does not move groundedness.
                    grounds: draft_ground.into_iter().collect(),
                    provenance: Provenance {
                        ingested_by: whoami(),
                        ingested_at: now,
                    },
                };
                self.write_fact(&fact)?;
                if recompute_centrality {
                    self.recompute_centrality()?;
                }
                self.commit(&tag, &claim.key())?;
                Ok(id)
            }

            WriteOp::Ground {
                fact: id, binding, ..
            } => {
                let mut fact = self.read_fact(&id, Rev::Working)?;
                let detail = format!(
                    "{} <- {}",
                    fact.claim.key(),
                    crate::ground::render_ground(&binding)
                );
                fact.grounds.push(binding);
                self.write_fact(&fact)?;
                self.commit(&tag, &detail)?;
                Ok(id)
            }

            WriteOp::GroundCheck {
                fact: id,
                ground,
                outcome,
                by,
                resolved,
                set_update,
                observed_cost,
                ..
            } => {
                let mut fact = self.read_fact(&id, Rev::Working)?;
                let now = Utc::now();
                let net = fact.grounds.get(ground).is_some_and(GroundBinding::is_net);

                // Record the replay state on the ground that was checked.
                if let (Some(g), Some(r)) = (fact.grounds.get_mut(ground), resolved.clone()) {
                    g.last = Some(r);
                }
                // Set-valued facts: replace elements with the diffed set (Step 9).
                if let Some(elements) = set_update {
                    fact.value = FactValue::Set { elements };
                }

                if outcome.updates_belief() {
                    let prior_var = decayed_variance(&fact, now, &self.config);
                    let prior = crate::decay::decayed_confidence(&fact, now, &self.config);
                    let confirmed = outcome == Outcome::Confirmed;
                    let (conf, var) = kalman_update(prior, prior_var, confirmed, net);
                    fact.epistemics.confidence = conf;
                    fact.schedule.last_verified = now;
                    fact.schedule.variance_at_verify = var;
                    self.append_calibration(&CalibrationSample {
                        prior,
                        grounded_outcome: confirmed,
                    })?;
                }
                // Record the measured run time for the generator that produced
                // this check, so VoI can use a quantile instead of the static
                // estimate (Idea 2). Only generator/handler sources carry a hash.
                if let (Some(h), Some(secs)) = (&by, observed_cost) {
                    self.record_cost(h, secs)?;
                }
                // Recompute groundedness from ALL grounds: independent grounds
                // that split between confirm and refute -> Conflicted (DESIGN §7).
                fact.epistemics.groundedness = aggregate_groundedness(&fact);

                self.write_fact(&fact)?;
                if recompute_centrality {
                    self.recompute_centrality()?;
                }
                let by = resolved
                    .as_ref()
                    .and_then(|r| r.by.as_ref().map(ToString::to_string))
                    .unwrap_or_else(|| "existence".to_string());
                self.commit(&tag, &format!("{} #{ground} {by}", fact.claim.key()))?;
                Ok(id)
            }

            WriteOp::Reschedule {
                fact: id,
                new_priority,
                ..
            } => {
                let mut fact = self.read_fact(&id, Rev::Working)?;
                fact.schedule.priority = new_priority;
                self.write_fact(&fact)?;
                self.commit(&tag, &fact.claim.key())?;
                Ok(id)
            }

            WriteOp::ManualOverride {
                fact: id,
                set,
                reason,
                ..
            } => {
                let mut fact = self.read_fact(&id, Rev::Working)?;
                fact.epistemics = set;
                self.write_fact(&fact)?;
                self.commit(&tag, &format!("{} :: {}", fact.claim.key(), reason))?;
                Ok(id)
            }
        }
    }
}

/// Recompute a fact's groundedness from all its grounds (DESIGN §7, §9). A
/// ground that last confirmed or refuted counts as "checked"; if independent
/// grounds split between confirm and refute, the fact holds two answers and is
/// `Conflicted`. With no checked ground it stays `Ungrounded` (preserving the
/// ingest triage source).
fn aggregate_groundedness(fact: &Fact) -> Groundedness {
    let placeholder = || GeneratorHash("existence".to_string());
    let mut any_confirm = false;
    let mut any_refute = false;
    let mut latest: Option<(Timestamp, GeneratorHash)> = None;
    let mut verifiers: Vec<GeneratorHash> = Vec::new();

    for g in &fact.grounds {
        let Some(last) = &g.last else { continue };
        let by = last.by.clone().unwrap_or_else(placeholder);
        match last.outcome {
            Outcome::Confirmed | Outcome::Refuted => {
                if last.outcome == Outcome::Confirmed {
                    any_confirm = true;
                } else {
                    any_refute = true;
                }
                verifiers.push(by.clone());
                if latest.as_ref().is_none_or(|(t, _)| last.at > *t) {
                    latest = Some((last.at, by));
                }
            }
            Outcome::Errored | Outcome::Inconclusive => {}
        }
    }

    match (any_confirm, any_refute) {
        (true, true) => Groundedness::Conflicted { verifiers },
        (false, false) => match &fact.epistemics.groundedness {
            Groundedness::Ungrounded { source } => Groundedness::Ungrounded {
                source: source.clone(),
            },
            other => other.clone(),
        },
        _ => {
            let (at, by) = latest.unwrap_or_else(|| (Utc::now(), placeholder()));
            Groundedness::Verified { at, by }
        }
    }
}

/// Parse the US/RS-delimited jj log template output into operations, one per
/// non-empty record. Callers apply their own filtering (empty descriptions,
/// `since` truncation).
fn parse_log_records(out: &str) -> Vec<Operation> {
    out.split(RS)
        .map(|record| record.trim_matches(|c: char| c == '\n' || c == '\r'))
        .filter(|record| !record.is_empty())
        .map(|record| {
            let mut parts = record.splitn(3, US);
            Operation {
                id: parts.next().unwrap_or("").trim().to_string(),
                time: parts.next().unwrap_or("").trim().to_string(),
                description: parts.next().unwrap_or("").trim().to_string(),
            }
        })
        .collect()
}

fn run_jj_in(dir: &Path, args: &[&str]) -> Result<String> {
    // Always operate on the given repo explicitly; never inherit ambient jj
    // discovery (README "Where the store lives").
    let mut full: Vec<&str> = vec!["-R", dir.to_str().unwrap_or(".")];
    full.extend_from_slice(args);
    run_jj_argv(dir, &full, args)
}

/// Run jj without the explicit `-R` (used only for `git init`, before the repo
/// exists). Operates in `dir` so the new repo lands there.
fn run_jj_raw(dir: &Path, args: &[&str]) -> Result<String> {
    run_jj_argv(dir, args, args)
}

fn run_jj_argv(dir: &Path, argv: &[&str], label: &[&str]) -> Result<String> {
    let output = Command::new("jj")
        .args(argv)
        .current_dir(dir)
        .output()
        .map_err(|e| Error::Jj(format!("spawning jj: {e}")))?;
    if !output.status.success() {
        return Err(Error::Jj(format!(
            "jj {}: {}",
            label.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "agent".to_string())
}

/// Is the `jj` CLI available on PATH? Used to gate integration tests.
pub fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Minimum supported `jj` version (the version ken is developed and tested
/// against). jj's CLI surface moves before 1.0, so the store refuses to run
/// against an older jj rather than failing obscurely mid-operation.
pub const JJ_MIN_VERSION: (u32, u32) = (0, 42);

/// Parse `jj --version` output ("jj 0.42.0") into `(major, minor)`.
fn parse_jj_version(output: &str) -> Option<(u32, u32)> {
    let rest = output.trim().strip_prefix("jj ")?;
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Enforce the jj version pin once per process (the result cannot change while
/// we run, so this is a perf-only cache).
fn ensure_jj_supported() -> Result<()> {
    use std::sync::OnceLock;
    static CHECK: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    CHECK
        .get_or_init(|| {
            let (want_major, want_minor) = JJ_MIN_VERSION;
            let output = Command::new("jj").arg("--version").output().map_err(|e| {
                format!("jj not found on PATH ({e}); ken requires jj >= {want_major}.{want_minor}")
            })?;
            let text = String::from_utf8_lossy(&output.stdout);
            let Some((major, minor)) = parse_jj_version(&text) else {
                return Err(format!(
                    "could not parse `jj --version` output {text:?}; ken requires jj >= {want_major}.{want_minor}"
                ));
            };
            if (major, minor) < (want_major, want_minor) {
                return Err(format!(
                    "jj {major}.{minor} is older than the supported minimum {want_major}.{want_minor}; \
                     upgrade jj (ken pins to a supported jj version until jj reaches 1.0)"
                ));
            }
            Ok(())
        })
        .clone()
        .map_err(Error::Jj)
}

#[cfg(test)]
mod tests {
    use super::{aggregate_groundedness, parse_jj_version, parse_log_records};
    use crate::ground::locator::parse_source;
    use crate::predicate::Predicate;
    use crate::schema::{Fact, GroundBinding, GroundSource, Groundedness, Resolved};
    use crate::test_support::verified_scalar;
    use crate::write::Outcome;
    use chrono::Utc;

    #[test]
    fn parses_jj_version_output() {
        assert_eq!(parse_jj_version("jj 0.42.0\n"), Some((0, 42)));
        assert_eq!(parse_jj_version("jj 1.0.3"), Some((1, 0)));
        assert_eq!(parse_jj_version("garbage"), None);
        assert_eq!(parse_jj_version(""), None);
    }

    fn ground_with(outcome: Option<Outcome>) -> GroundBinding {
        let (src, locator) = parse_source("store:data.txt", None).unwrap();
        GroundBinding {
            source: GroundSource::File(src),
            locator,
            predicate: Predicate::Exists,
            last: outcome.map(|outcome| Resolved {
                rev: String::new(),
                span_hash: String::new(),
                at: Utc::now(),
                outcome,
                by: None,
            }),
        }
    }

    fn fact_with_grounds(grounds: Vec<GroundBinding>) -> Fact {
        let mut fact = verified_scalar("svc.url");
        fact.grounds = grounds;
        fact
    }

    #[test]
    fn confirm_and_refute_aggregate_to_conflicted() {
        let fact = fact_with_grounds(vec![
            ground_with(Some(Outcome::Confirmed)),
            ground_with(Some(Outcome::Refuted)),
        ]);
        assert!(matches!(
            aggregate_groundedness(&fact),
            Groundedness::Conflicted { .. }
        ));
    }

    #[test]
    fn all_confirm_aggregates_to_verified() {
        let fact = fact_with_grounds(vec![
            ground_with(Some(Outcome::Confirmed)),
            ground_with(Some(Outcome::Confirmed)),
        ]);
        assert!(matches!(
            aggregate_groundedness(&fact),
            Groundedness::Verified { .. }
        ));
    }

    #[test]
    fn errored_and_unchecked_grounds_do_not_move_groundedness() {
        // Errored is not a refutation (the channel failed, not the claim), and
        // a draft binding with no check yet counts for nothing.
        let mut fact =
            fact_with_grounds(vec![ground_with(Some(Outcome::Errored)), ground_with(None)]);
        fact.epistemics.groundedness = Groundedness::Ungrounded {
            source: crate::schema::TriageSource::Ingest,
        };
        assert!(matches!(
            aggregate_groundedness(&fact),
            Groundedness::Ungrounded { .. }
        ));
    }

    #[test]
    fn parse_log_records_splits_on_separators() {
        let us = '\u{1f}';
        let rs = '\u{1e}';
        let out =
            format!("op1{us}t1{us}[Ingest] a.b{rs}op2{us}t2{us}[GroundCheck:confirmed] a.b{rs}");
        let records = parse_log_records(&out);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, "op1");
        assert_eq!(records[1].description, "[GroundCheck:confirmed] a.b");
    }
}
