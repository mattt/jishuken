//! The storage seam (DESIGN §4). A fact is a file at
//! `facts/<entity>/<relation>.json`, so the store greps and diffs like code.
//! Every [`WriteOp`] lands as one tagged record in an append-only op log
//! (`ops.jsonl`) that `ken` owns: the log is the audit, the fact files are the
//! working state, and `undo` replays a record's saved bytes in reverse. Writes
//! serialize behind a store lock; single-writer is the supported posture.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::calibration::CalibrationSample;
use crate::centrality::FactGraph;
use crate::config::Config;
use crate::decay::{decayed_variance, kalman_update};
use crate::error::{Error, Result};
use crate::ground::generator::GeneratorRegistry;
use crate::schema::{
    path_for, Claim, Epistemics, Fact, FactId, FactValue, GeneratorHash, GroundBinding,
    Groundedness, Provenance, ScheduleMeta, Timestamp,
};
use crate::sketch::TDigest;
use crate::write::{landed_confidence_with, Outcome, WriteOp};

/// An op-log record id (blake3 prefix over the previous id, tag, detail, time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpId(pub String);

/// One entry in the op log, as read back for `ken log` / `ken why`. The
/// `description` is `[Tag] detail`, so the audit stays greppable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub id: String,
    pub time: String,
    pub description: String,
}

/// Initial posterior variance for a freshly ingested, unverified fact: high, so
/// the first verifier run moves it a lot.
const INGEST_VARIANCE: f64 = 0.25;

/// A lock older than this is treated as abandoned by a crashed writer and
/// stolen. The lock is held only for the duration of a single write, so a fresh
/// lock always means a live writer.
const STALE_LOCK: Duration = Duration::from_secs(30);

/// One file touched by an op: its path (relative to the store root) and the
/// bytes before and after. `before: None` means the op created the file;
/// `undo` restores `before`, deleting the file when it is `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Change {
    path: String,
    before: Option<String>,
    after: Option<String>,
}

/// One op-log record. Serialized as a single line in `ops.jsonl`, oldest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpRecord {
    id: String,
    time: String,
    tag: String,
    detail: String,
    #[serde(default)]
    changes: Vec<Change>,
}

#[derive(Debug)]
pub struct KenStore {
    root: PathBuf,
    config: Config,
    /// Changes accumulated by the in-flight op, drained when it is committed.
    pending: Mutex<Vec<Change>>,
}

impl KenStore {
    /// Discover the store by walking up from `start` for a `.ken/` directory,
    /// the way `git` finds `.git/`. Honors an explicit override first.
    ///
    /// # Errors
    /// Returns [`Error::StoreNotFound`] if no `.ken/` store is found.
    pub fn discover(explicit: Option<&Path>, start: &Path) -> Result<KenStore> {
        if let Some(p) = explicit {
            return KenStore::open(p);
        }
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(".ken");
            if candidate.join("ken.toml").is_file() {
                return KenStore::open(&candidate);
            }
            dir = d.parent();
        }
        Err(Error::StoreNotFound)
    }

    /// Open an existing `.ken/` store at `root`.
    ///
    /// # Errors
    /// Returns [`Error::StoreNotFound`] if `root` is not a `ken` store (no
    /// `ken.toml`).
    pub fn open(root: &Path) -> Result<KenStore> {
        if !root.join("ken.toml").is_file() {
            return Err(Error::StoreNotFound);
        }
        let config = Config::load_or_default(&root.join("ken.toml"));
        Ok(KenStore {
            root: root.to_path_buf(),
            config,
            pending: Mutex::new(Vec::new()),
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

    /// Create a `.ken/` store: `ken.toml`, `facts/`, `verifiers/`, and an op log
    /// seeded with one `[Init]` record.
    ///
    /// # Errors
    /// Returns an error if the directories or files cannot be created.
    pub fn init(root: &Path) -> Result<KenStore> {
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(root.join("facts"))?;
        std::fs::create_dir_all(root.join("verifiers"))?;
        let cfg = Config::default();
        std::fs::write(root.join("ken.toml"), cfg.to_toml())?;
        let store = KenStore::open(root)?;
        {
            let _lock = store.lock()?;
            store.append_op("Init", "ken store")?;
        }
        Ok(store)
    }

    fn ops_path(&self) -> PathBuf {
        self.root.join("ops.jsonl")
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

    /// Read a fact by id from the working copy.
    fn read_fact(&self, id: &FactId) -> Result<Fact> {
        self.read_fact_by_key(&id.0)
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

    /// Write a fact file, recording the before/after bytes on the in-flight op
    /// so it can be undone.
    fn write_fact(&self, fact: &Fact) -> Result<()> {
        let path = self.fact_path(&fact.claim);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let before = std::fs::read_to_string(&path).ok();
        let after = String::from_utf8(serde_json::to_vec_pretty(fact)?)
            .expect("serde_json emits valid utf-8");
        std::fs::write(&path, &after)?;
        self.pending.lock().expect("pending lock").push(Change {
            path: path_for(&fact.claim),
            before,
            after: Some(after),
        });
        Ok(())
    }

    /// Persist a fact file, recording it on the in-flight op. Control-plane
    /// callers use this alongside [`KenStore::control_commit`] to land one
    /// tagged op.
    ///
    /// # Errors
    /// Returns an error if the fact file cannot be written.
    pub fn put_fact(&self, fact: &Fact) -> Result<()> {
        self.write_fact(fact)
    }

    /// Append one op-log record, draining the changes the in-flight op recorded.
    fn append_op(&self, tag: &str, detail: &str) -> Result<()> {
        let changes = std::mem::take(&mut *self.pending.lock().expect("pending lock"));
        let time = Utc::now().to_rfc3339();
        let prev = self.last_op_id()?;
        let seed = format!("{prev}\u{1f}{tag}\u{1f}{detail}\u{1f}{time}");
        let id = blake3::hash(seed.as_bytes()).to_hex()[..12].to_string();
        let rec = OpRecord {
            id,
            time,
            tag: tag.to_string(),
            detail: detail.to_string(),
            changes,
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.ops_path())?;
        writeln!(f, "{}", serde_json::to_string(&rec)?)?;
        f.sync_all()?;
        Ok(())
    }

    /// Commit the in-flight op under a tag (private; every write path ends here).
    fn commit(&self, tag: &str, detail: &str) -> Result<()> {
        self.append_op(tag, detail)
    }

    /// Control-plane tagged commit (e.g. `Grant`), one loud logged operation.
    ///
    /// # Errors
    /// Returns an error if the op log cannot be appended.
    pub fn control_commit(&self, tag: &str, detail: &str) -> Result<()> {
        self.append_op(tag, detail)
    }

    /// Read the op log, oldest first. A missing log reads as empty.
    fn read_ops(&self) -> Result<Vec<OpRecord>> {
        let text = match std::fs::read_to_string(self.ops_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    /// The id of the newest op-log record, or an empty string if the log is empty.
    fn last_op_id(&self) -> Result<String> {
        Ok(self
            .read_ops()?
            .last()
            .map_or_else(String::new, |r| r.id.clone()))
    }

    fn append_calibration(&self, sample: &CalibrationSample) -> Result<()> {
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

    /// Roll the store back one operation (`ken undo`): restore the saved bytes
    /// of the last non-`Undo` op, then log an `[Undo]` record.
    ///
    /// # Errors
    /// Returns an error if the log cannot be read or a file cannot be restored.
    pub fn undo(&self) -> Result<()> {
        let _lock = self.lock()?;
        let recs = self.read_ops()?;
        let Some(rec) = recs.iter().rev().find(|r| r.tag != "Undo") else {
            return Err(Error::Store("nothing to undo".into()));
        };
        // Replay in reverse so the earliest `before` for a path wins, leaving
        // each file as it was before the op ran.
        for ch in rec.changes.iter().rev() {
            let path = self.root.join(&ch.path);
            match &ch.before {
                Some(bytes) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, bytes)?;
                }
                None => {
                    if path.exists() {
                        std::fs::remove_file(&path)?;
                    }
                }
            }
        }
        let detail = format!("{} {}", rec.tag, rec.detail);
        self.append_op("Undo", detail.trim())?;
        Ok(())
    }

    /// The operation log, newest first, optionally truncated at `since`
    /// (keeping only records strictly newer than the `since` id).
    ///
    /// # Errors
    /// Returns an error if the op log cannot be read.
    pub fn op_log(&self, since: Option<OpId>) -> Result<Vec<Operation>> {
        let mut ops: Vec<Operation> = self
            .read_ops()?
            .iter()
            .rev()
            .map(|r| Operation {
                id: r.id.clone(),
                time: r.time.clone(),
                description: format!("[{}] {}", r.tag, r.detail),
            })
            .collect();
        if let Some(OpId(stop)) = since {
            if let Some(pos) = ops.iter().position(|o| o.id == stop) {
                ops.truncate(pos);
            }
        }
        Ok(ops)
    }

    /// The facts currently in a `Conflicted` state.
    ///
    /// # Errors
    /// Returns an error if the facts cannot be read.
    pub fn list_conflicts(&self) -> Result<Vec<FactId>> {
        Ok(self
            .all_facts()?
            .into_iter()
            .filter(|f| matches!(f.epistemics.groundedness, Groundedness::Conflicted { .. }))
            .map(|f| f.id)
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

    /// Apply one [`WriteOp`], landing it as one tagged op-log record.
    ///
    /// # Errors
    /// Returns an error if the store cannot be read or written.
    pub fn apply(&self, op: WriteOp) -> Result<FactId> {
        self.apply_op(op, true)
    }

    /// Apply a write op without recomputing centrality. The scheduler tick
    /// (Idea 1) uses this to coalesce N per-check recomputes into one after the
    /// loop; `recompute_centrality` is then called once and committed separately.
    ///
    /// # Errors
    /// Returns an error if the underlying write fails.
    pub fn apply_no_centrality(&self, op: WriteOp) -> Result<FactId> {
        self.apply_op(op, false)
    }

    fn apply_op(&self, op: WriteOp, recompute_centrality: bool) -> Result<FactId> {
        let _lock = self.lock()?;
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
                let id = FactId::for_claim(&claim);
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
                let mut fact = self.read_fact(&id)?;
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
                let mut fact = self.read_fact(&id)?;
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
                let mut fact = self.read_fact(&id)?;
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
                let mut fact = self.read_fact(&id)?;
                fact.epistemics = set;
                self.write_fact(&fact)?;
                self.commit(&tag, &format!("{} :: {}", fact.claim.key(), reason))?;
                Ok(id)
            }
        }
    }

    /// Take the store's exclusive write lock. A crashed writer's lock (older
    /// than [`STALE_LOCK`]) is stolen; a fresh one is refused.
    fn lock(&self) -> Result<LockGuard> {
        let path = self.root.join("lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(LockGuard { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().map(|d| d > STALE_LOCK).unwrap_or(true))
                        .unwrap_or(false);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    let pid = std::fs::read_to_string(&path).unwrap_or_default();
                    return Err(Error::Store(format!(
                        "store is locked by pid {}; remove {} if no ken is running",
                        pid.trim(),
                        path.display()
                    )));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Store("could not acquire store lock".into()))
    }
}

/// Releases the store lock file on drop.
#[derive(Debug)]
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "agent".to_string())
}

#[cfg(test)]
mod tests {
    use super::{aggregate_groundedness, KenStore, OpId};
    use crate::ground::locator::parse_source;
    use crate::predicate::Predicate;
    use crate::schema::{
        Claim, Fact, FactValue, GroundBinding, GroundSource, Groundedness, Resolved, TriageSource,
        Volatility,
    };
    use crate::test_support::verified_scalar;
    use crate::write::{Outcome, WriteOp};
    use chrono::Utc;

    fn scalar(value: &str) -> FactValue {
        FactValue::Scalar {
            value: serde_json::Value::String(value.into()),
        }
    }

    fn ingest(store: &KenStore, key: &str) {
        let claim = Claim::parse_key(key).unwrap();
        store
            .apply(WriteOp::ingest(
                claim,
                scalar("v"),
                TriageSource::Ingest,
                Volatility::Days,
                None,
            ))
            .unwrap();
    }

    #[test]
    fn ingest_appends_a_tagged_op_and_undo_reverts_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = KenStore::init(&dir.path().join(".ken")).unwrap();

        ingest(&store, "db.host");
        assert!(store.read_fact_by_key("db.host").is_ok());
        let log = store.op_log(None).unwrap();
        assert!(
            log.iter()
                .any(|o| o.description.contains("[Ingest]") && o.description.contains("db.host")),
            "op log should carry a tagged Ingest: {log:?}"
        );

        store.undo().unwrap();
        assert!(
            store.read_fact_by_key("db.host").is_err(),
            "undo should remove the created fact file"
        );
        assert!(
            store
                .op_log(None)
                .unwrap()
                .iter()
                .any(|o| o.description.contains("[Undo]")),
            "undo is itself recorded in the op log"
        );
    }

    #[test]
    fn op_log_since_keeps_only_newer_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = KenStore::init(&dir.path().join(".ken")).unwrap();

        ingest(&store, "x.one");
        let marker = store.op_log(None).unwrap().first().unwrap().id.clone();
        ingest(&store, "x.two");

        let since = store.op_log(Some(OpId(marker))).unwrap();
        assert!(since.iter().all(|o| !o.description.contains("x.one")));
        assert!(since.iter().any(|o| o.description.contains("x.two")));
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
}
