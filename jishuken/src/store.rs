//! Fact files and operation history.
//! A fact is a file at `facts/<entity>/<relation>.json`, so the store greps and
//! diffs like code.
//! Every [`WriteOp`] lands as one tagged record in an append-only op log
//! (`ops.jsonl`) that `ken` owns: the log is the audit, the fact files are the
//! working state, and `undo` replays a record's saved bytes in reverse.
//! Writes serialize behind a store lock; single-writer is the supported
//! posture.

mod transaction;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::calibration::CalibrationSample;
use crate::centrality::FactGraph;
use crate::config::Config;
use crate::decay::{decayed_variance, kalman_update};
use crate::error::{Error, Result};
use crate::ground::generator::GeneratorRegistry;
#[cfg(test)]
use crate::schema::legacy_path_for;
use crate::schema::{
    path_for, Claim, Epistemics, Fact, FactId, FactValue, GeneratorHash, GroundBinding,
    Groundedness, Provenance, ScheduleMeta, Timestamp,
};
use crate::sketch::TDigest;
use crate::write::{landed_confidence_with, Outcome, WriteOp};
use transaction::{read_optional, LogUpdate};

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
pub struct JishukenStore {
    root: PathBuf,
    config: Config,
    /// Changes accumulated by the in-flight op, drained when it is committed.
    pending: Mutex<Vec<Change>>,
}

impl JishukenStore {
    /// Discover the store by walking up from `start` for a `.ken/` directory,
    /// the way `git` finds `.git/`. Honors an explicit override first.
    ///
    /// # Errors
    /// Returns [`Error::StoreNotFound`] if no `.ken/` store is found.
    pub fn discover(explicit: Option<&Path>, start: &Path) -> Result<JishukenStore> {
        if let Some(p) = explicit {
            return JishukenStore::open(p);
        }
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(".ken");
            if candidate.join("ken.toml").is_file() {
                return JishukenStore::open(&candidate);
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
    pub fn open(root: &Path) -> Result<JishukenStore> {
        if !root.join("ken.toml").is_file() {
            return Err(Error::StoreNotFound);
        }
        let config = Config::load_or_default(&root.join("ken.toml"));
        let store = JishukenStore {
            root: root.to_path_buf(),
            config,
            pending: Mutex::new(Vec::new()),
        };
        // Recover interrupted commits before exposing the working state.
        {
            let _lock = store.lock()?;
        }
        Ok(store)
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
    pub fn init(root: &Path) -> Result<JishukenStore> {
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(root.join("facts"))?;
        std::fs::create_dir_all(root.join("verifiers"))?;
        let cfg = Config::default();
        std::fs::write(root.join("ken.toml"), cfg.to_toml())?;
        let store = JishukenStore::open(root)?;
        {
            let _lock = store.lock()?;
            store.append_op("Init", "ken store")?;
        }
        Ok(store)
    }

    fn ops_path(&self) -> PathBuf {
        self.root.join("ops.jsonl")
    }

    fn read_text(&self, path: &str) -> Result<Option<String>> {
        if let Some(change) = self
            .pending
            .lock()
            .expect("pending lock")
            .iter()
            .find(|c| c.path == path)
        {
            return Ok(change.after.clone());
        }
        read_optional(&self.root.join(path))
    }

    /// Read a fact by its `entity.relation` key from the working copy.
    ///
    /// # Errors
    /// Returns an error if the key is malformed, the fact does not exist, or its
    /// file is not valid JSON.
    pub fn read_fact_by_key(&self, key: &str) -> Result<Fact> {
        let key = Claim::parse_key(key)?.key();
        self.all_facts()?
            .into_iter()
            .find(|f| f.claim.key() == key)
            .ok_or(Error::FactNotFound(key))
    }

    /// Read a fact by id from the working copy.
    fn read_fact(&self, id: &FactId) -> Result<Fact> {
        self.read_fact_by_key(&id.0)
    }

    /// All facts currently in the store (scans `facts/`).
    ///
    /// # Errors
    /// Returns an error if the `facts/` directory cannot be read.
    ///
    /// # Panics
    /// Panics if another operation panicked while staging changes.
    pub fn all_facts(&self) -> Result<Vec<Fact>> {
        let mut seen = std::collections::HashSet::new();
        self.fact_files()?.into_iter().map(|(_, fact)| {
            if !seen.insert(fact.id.clone()) {
                return Err(Error::Store(format!("multiple files normalize to key {}; resolve the duplicate before continuing", fact.id)));
            }
            Ok(fact)
        }).collect()
    }

    fn fact_files(&self) -> Result<Vec<(String, Fact)>> {
        let mut files = HashMap::new();
        let facts_dir = self.root.join("facts");
        if !facts_dir.is_dir() {
            return Err(Error::Store("facts directory is missing".into()));
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
                let path = rel.path();
                let relative = path
                    .strip_prefix(&self.root)
                    .map_err(|e| Error::Store(e.to_string()))?
                    .to_string_lossy()
                    .into_owned();
                files.insert(relative, std::fs::read_to_string(path)?);
            }
        }
        for change in self.pending.lock().expect("pending lock").iter() {
            if !change.path.starts_with("facts/") {
                continue;
            }
            if let Some(text) = &change.after {
                files.insert(change.path.clone(), text.clone());
            } else {
                files.remove(&change.path);
            }
        }
        let mut out = Vec::new();
        let mut paths: Vec<_> = files.into_iter().collect();
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, text) in paths {
            out.push((path, self.decode_fact(&text)?));
        }
        Ok(out)
    }

    /// Identify old ingest hints from history, preserving explicit ground bindings.
    fn decode_fact(&self, text: &str) -> Result<Fact> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        let legacy = value.get("source_hint").is_none();
        let mut fact: Fact = serde_json::from_value(value)?;
        fact.claim.normalize();
        if Claim::parse_key(&fact.id.0)?.key() != fact.claim.key() {
            return Err(Error::Store("fact ID does not match its claim".into()));
        }
        fact.id = FactId::for_claim(&fact.claim);
        if !legacy || fact.grounds.is_empty() {
            return Ok(fact);
        }
        // Old Ingest snapshots are the evidence that the first ground was a hint.
        // An explicitly bound `exists` predicate must remain an active ground.
        let records = self.read_ops()?;
        let Some(ingest) = records.iter().rev().find(|r| {
            r.tag == "Ingest"
                && Claim::parse_key(&r.detail).is_ok_and(|c| c.key() == fact.claim.key())
        }) else {
            return Ok(fact);
        };
        let ingested = ingest
            .changes
            .iter()
            .filter_map(|c| c.after.as_deref())
            .filter_map(|text| serde_json::from_str::<Fact>(text).ok())
            .find(|saved| saved.claim.key() == fact.claim.key());
        let Some(ingested) = ingested else {
            return Ok(fact);
        };
        let Some(hint) = ingested.grounds.first() else {
            return Ok(fact);
        };
        let first = &fact.grounds[0];
        if first.source == hint.source
            && first.locator == hint.locator
            && first.predicate == hint.predicate
        {
            let mut hint = fact.grounds.remove(0);
            hint.last = None;
            fact.source_hint = Some(hint);
            // Discard belief updates that included the existence-only hint.
            fact.epistemics = ingested.epistemics;
            fact.schedule = ingested.schedule;
            for ground in &mut fact.grounds {
                ground.last = None;
            }
        }
        Ok(fact)
    }

    /// Stage a fact and migrate its old filename when necessary.
    fn write_fact(&self, fact: &Fact) -> Result<()> {
        let mut fact = fact.clone();
        fact.claim.normalize();
        fact.id = FactId::for_claim(&fact.claim);
        let fact = &fact;
        let path = path_for(&fact.claim);
        let aliases: Vec<_> = self
            .fact_files()?
            .into_iter()
            .filter(|(_, saved)| saved.id == fact.id)
            .collect();
        if aliases.len() > 1 {
            return Err(Error::Store(format!(
                "multiple files normalize to key {}; resolve the duplicate before continuing",
                fact.id
            )));
        }
        for (old_path, _) in aliases {
            if old_path != path {
                self.stage_file(&old_path, None)?;
            }
        }
        if let Some(text) = self.read_text(&path)? {
            let saved = self.decode_fact(&text)?;
            if saved.claim.key() != fact.claim.key() || saved.id != fact.id {
                if path == path_for(&saved.claim) {
                    return Err(Error::Store(format!("{path} belongs to a different fact")));
                }
                self.write_fact(&saved)?;
            }
        }
        self.stage_file(&path, Some(serde_json::to_string_pretty(fact)?))
    }

    fn stage_file(&self, path: &str, after: Option<String>) -> Result<()> {
        let mut pending = self.pending.lock().expect("pending lock");
        if let Some(change) = pending.iter_mut().find(|c| c.path == path) {
            change.after = after;
        } else {
            pending.push(Change {
                path: path.to_string(),
                before: read_optional(&self.root.join(path))?,
                after,
            });
        }
        Ok(())
    }

    /// Stage a fact for the next [`JishukenStore::control_commit`].
    ///
    /// # Errors
    /// Returns an error if the fact file cannot be written.
    pub fn put_fact(&self, fact: &Fact) -> Result<()> {
        let result = self.write_fact(fact);
        if result.is_err() {
            self.discard_pending();
        }
        result
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
        let line = format!("{}\n", serde_json::to_string(&rec)?);
        transaction::commit(&self.root, rec.changes, LogUpdate::Append(line))
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
        let result = (|| {
            let _lock = self.lock()?;
            self.append_op(tag, detail)
        })();
        self.discard_pending();
        result
    }

    fn discard_pending(&self) {
        self.pending.lock().expect("pending lock").clear();
    }

    /// Read the op log, oldest first. A missing log reads as empty.
    fn read_ops(&self) -> Result<Vec<OpRecord>> {
        let text = match std::fs::read_to_string(self.ops_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        text.lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| {
                serde_json::from_str(l).map_err(|e| {
                    Error::Store(format!("invalid ops.jsonl record on line {}: {e}", i + 1))
                })
            })
            .collect()
    }

    /// The id of the newest op-log record, or an empty string if the log is empty.
    fn last_op_id(&self) -> Result<String> {
        Ok(self
            .read_ops()?
            .last()
            .map_or_else(String::new, |r| r.id.clone()))
    }

    fn append_calibration(&self, sample: &CalibrationSample) -> Result<()> {
        let mut text = self.read_text("calibration.jsonl")?.unwrap_or_default();
        text.push_str(&serde_json::to_string(sample)?);
        text.push('\n');
        self.stage_file("calibration.jsonl", Some(text))
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

    /// Stage an observed run time for the next commit, keyed by generator hash.
    ///
    /// # Errors
    /// Returns an error if the cost sketch file cannot be written.
    pub fn record_cost(&self, by: &GeneratorHash, secs: f64) -> Result<()> {
        let mut costs = self.load_costs();
        costs.entry(by.0.clone()).or_default().insert(secs);
        self.stage_file("costs.json", Some(serde_json::to_string_pretty(&costs)?))
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

    /// Restore the last operation's saved bytes and remove it from history.
    ///
    /// # Errors
    /// Returns an error if the log cannot be read or a file cannot be restored.
    ///
    /// # Panics
    /// Panics if another operation panicked while staging changes.
    pub fn undo(&self) -> Result<()> {
        let _lock = self.lock()?;
        let mut recs = self.read_ops()?;
        // Earlier versions appended Undo records without consuming the operation.
        // A trailing group of those records has already reversed its preceding op.
        while recs.last().is_some_and(|r| r.tag == "Undo") {
            while recs.last().is_some_and(|r| r.tag == "Undo") {
                recs.pop();
            }
            recs.pop();
        }
        let Some(rec) = recs.pop().filter(|r| r.tag != "Init") else {
            return Err(Error::Store("nothing to undo".into()));
        };
        // Replay in reverse so the earliest `before` for a path wins, leaving
        // each file as it was before the op ran.
        let result = (|| {
            for ch in rec.changes.iter().rev() {
                self.stage_file(&ch.path, ch.before.clone())?;
            }
            let mut log = String::new();
            for record in recs {
                log.push_str(&serde_json::to_string(&record)?);
                log.push('\n');
            }
            let changes = std::mem::take(&mut *self.pending.lock().expect("pending lock"));
            transaction::commit(&self.root, changes, LogUpdate::Replace(log))
        })();
        self.discard_pending();
        result
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

    /// Recompute trust-weighted centrality over a fact→entity graph: an entity
    /// is central in proportion to the *grounded* facts that touch it, and each
    /// fact inherits the centrality of its entity.
    /// Ungrounded facts contribute zero, so data-plane injection cannot inflate
    /// the budget.
    ///
    /// Returns `true` if any fact's centrality changed (and was rewritten), so
    /// callers that coalesce the recompute can skip an empty commit.
    ///
    /// # Errors
    /// Returns an error if the facts cannot be read or rewritten.
    pub fn recompute_centrality(&self) -> Result<bool> {
        let result = self.stage_centrality();
        if result.is_err() {
            self.discard_pending();
        }
        result
    }

    fn stage_centrality(&self) -> Result<bool> {
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
        let result = self.apply_staged(op, recompute_centrality);
        self.discard_pending();
        result
    }

    fn apply_staged(&self, op: WriteOp, recompute_centrality: bool) -> Result<FactId> {
        let tag = op.tag();
        match op {
            WriteOp::Ingest {
                mut claim,
                value,
                triage,
                volatility,
                draft_ground,
            } => {
                claim.normalize();
                let now = Utc::now();
                let id = FactId::for_claim(&claim);
                // Calibrate the LLM triage that produced this prior. The
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
                    grounds: Vec::new(),
                    source_hint: draft_ground.map(|mut hint| {
                        hint.last = None;
                        hint
                    }),
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
                fact.source_hint = None;
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
                // that split between confirm and refute -> Conflicted.
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

    /// Hold an OS lock until the returned file closes, including on process exit.
    fn lock(&self) -> Result<std::fs::File> {
        let path = self.root.join("lock");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Error::Store(
                    "store is locked by another writer; retry when it finishes".into(),
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        file.set_len(0)?;
        write!(file, "{}", std::process::id())?;
        transaction::recover(&self.root)?;
        Ok(file)
    }
}

/// Recompute a fact's groundedness from all its grounds.
/// A ground that last confirmed or refuted counts as "checked"; if independent
/// grounds split between confirm and refute, the fact holds two answers and is
/// `Conflicted`.
/// If all checked grounds refute the claim, it is `Refuted`.
/// With no checked ground it stays `Ungrounded` (preserving the ingest triage
/// source).
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
            if any_confirm {
                Groundedness::Verified { at, by }
            } else {
                Groundedness::Refuted { at, by }
            }
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
    use super::{aggregate_groundedness, legacy_path_for, JishukenStore, OpId};
    use crate::ground::locator::parse_source;
    use crate::predicate::Predicate;
    use crate::schema::{
        Claim, Fact, FactValue, GeneratorHash, GroundBinding, GroundSource, Groundedness, Resolved,
        TriageSource, Volatility,
    };
    use crate::test_support::verified_scalar;
    use crate::write::{Outcome, WriteOp};
    use chrono::Utc;

    fn scalar(value: &str) -> FactValue {
        FactValue::Scalar {
            value: serde_json::Value::String(value.into()),
        }
    }

    fn ingest(store: &JishukenStore, key: &str) {
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
        let store = JishukenStore::init(&dir.path().join(".ken")).unwrap();

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
        assert_eq!(store.op_log(None).unwrap().len(), 1);
        assert!(store.undo().is_err(), "Init is not an undoable operation");
    }

    #[test]
    fn keys_remain_distinct_on_case_insensitive_filesystems() {
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(dir.path()).unwrap();
        let keys = [
            "service.api.owner",
            "service_api.owner",
            "Service.api.owner",
            "service/api.owner",
            "service%2Eapi.owner",
            "日本.owner",
            "語.owner",
            "release.Owner",
            "release.owner",
            "../../escape.value",
        ];
        let mut paths = std::collections::HashSet::new();
        for key in keys {
            store
                .apply(WriteOp::ingest(
                    Claim::parse_key(key).unwrap(),
                    scalar(key),
                    TriageSource::Ingest,
                    Volatility::Days,
                    None,
                ))
                .unwrap();
            let path = super::path_for(&Claim::parse_key(key).unwrap());
            assert!(paths.insert(path.to_lowercase()));
            assert!(store.root().join(path).is_file());
        }
        for key in keys {
            assert_eq!(store.read_fact_by_key(key).unwrap().value.render(), key);
        }
        assert_eq!(store.all_facts().unwrap().len(), keys.len());
    }

    #[test]
    fn legacy_collision_migrates_without_losing_either_fact_and_can_be_undone() {
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(dir.path()).unwrap();
        ingest(&store, "service.api.owner");
        let claim = Claim::parse_key("service.api.owner").unwrap();
        let encoded = store.root().join(super::path_for(&claim));
        let legacy = store.root().join(super::legacy_path_for(&claim));
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::rename(&encoded, &legacy).unwrap();
        assert!(store.read_fact_by_key("service_api.owner").is_err());
        assert!(store.read_fact_by_key("service.api.owner").is_ok());
        ingest(&store, "service_api.owner");
        assert!(encoded.is_file());
        assert!(store.read_fact_by_key("service_api.owner").is_ok());
        assert!(store.read_fact_by_key("service.api.owner").is_ok());
        assert_eq!(store.all_facts().unwrap().len(), 2);
        store.undo().unwrap();
        assert!(!encoded.exists());
        assert!(legacy.is_file());
        assert!(store.read_fact_by_key("service_api.owner").is_err());
        assert_eq!(store.all_facts().unwrap().len(), 1);
    }

    #[test]
    fn log_failure_leaves_facts_unchanged_and_does_not_leak_into_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(dir.path()).unwrap();
        ingest(&store, "release.owner");
        let before = store.read_fact_by_key("release.owner").unwrap();
        let log = store.ops_path();
        let backup = store.root().join("ops.backup");
        std::fs::rename(&log, &backup).unwrap();
        std::fs::create_dir(&log).unwrap();
        assert!(store
            .apply(WriteOp::ingest(
                before.claim.clone(),
                scalar("new"),
                TriageSource::Ingest,
                Volatility::Days,
                None
            ))
            .is_err());
        assert_eq!(store.read_fact_by_key("release.owner").unwrap(), before);
        assert!(store.pending.lock().unwrap().is_empty());
        std::fs::remove_dir(&log).unwrap();
        std::fs::rename(backup, log).unwrap();
        ingest(&store, "other.owner");
        store.undo().unwrap();
        assert_eq!(store.read_fact_by_key("release.owner").unwrap(), before);
        assert_eq!(store.op_log(None).unwrap().len(), 2);
    }

    #[test]
    fn malformed_log_is_reported_before_writing_a_fact() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(dir.path()).unwrap();
        ingest(&store, "release.owner");
        let before = store.read_fact_by_key("release.owner").unwrap();
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(store.ops_path())
            .unwrap();
        writeln!(log, "{{broken record").unwrap();
        let error = store
            .apply(WriteOp::ingest(
                before.claim.clone(),
                scalar("new"),
                TriageSource::Ingest,
                Volatility::Days,
                None,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("line 3"));
        assert_eq!(store.read_fact_by_key("release.owner").unwrap(), before);
    }

    #[test]
    fn canonical_unicode_keys_share_identity_and_undo_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(dir.path()).unwrap();
        for (composed, decomposed) in [
            ("café.rôle", "cafe\u{0301}.ro\u{0302}le"),
            ("각.owner", "\u{1100}\u{1161}\u{11a8}.owner"),
            ("q\u{0323}\u{0307}.owner", "q\u{0307}\u{0323}.owner"),
        ] {
            let first = Claim::parse_key(composed).unwrap();
            let second = Claim::parse_key(decomposed).unwrap();
            assert_eq!(first, second);
            assert_eq!(super::path_for(&first), super::path_for(&second));
            // Construct an unnormalized claim directly to exercise the library boundary.
            let (entity, relation) = decomposed.rsplit_once('.').unwrap();
            let raw = Claim {
                entity: crate::schema::EntityId(entity.into()),
                relation: relation.into(),
                nl: None,
            };
            let id = store
                .apply(WriteOp::ingest(
                    raw,
                    scalar("first"),
                    TriageSource::Ingest,
                    Volatility::Days,
                    None,
                ))
                .unwrap();
            assert_eq!(id.0, first.key());
            let fact = store.read_fact_by_key(composed).unwrap();
            assert_eq!(fact.claim, first);
            store
                .apply(WriteOp::ingest(
                    second,
                    scalar("second"),
                    TriageSource::Ingest,
                    Volatility::Days,
                    None,
                ))
                .unwrap();
            assert_eq!(
                store.read_fact_by_key(decomposed).unwrap().value.render(),
                "second"
            );
            store.undo().unwrap();
            assert_eq!(store.read_fact_by_key(decomposed).unwrap(), fact);
        }
        assert_eq!(store.all_facts().unwrap().len(), 3);
        for key in [
            "café.owner",
            "Café.owner",
            "①.owner",
            "1.owner",
            "Ａ.owner",
            "A.owner",
        ] {
            ingest(&store, key);
        }
        assert_eq!(store.all_facts().unwrap().len(), 9);
    }

    #[test]
    fn non_normalized_legacy_keys_migrate_and_duplicates_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(dir.path()).unwrap();
        let mut fact = verified_scalar("café.owner");
        fact.claim.entity.0 = "cafe\u{0301}".into();
        fact.id.0 = "cafe\u{0301}.owner".into();
        let legacy = dir.path().join(legacy_path_for(&fact.claim));
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, serde_json::to_string(&fact).unwrap()).unwrap();
        assert_eq!(
            store.read_fact_by_key("café.owner").unwrap().id.0,
            "café.owner"
        );
        ingest(&store, "café.owner");
        assert!(!legacy.exists());
        let canonical = dir.path().join(super::path_for(&fact.claim));
        let before = std::fs::read(&canonical).unwrap();
        std::fs::write(&legacy, serde_json::to_string(&fact).unwrap()).unwrap();
        assert!(store
            .read_fact_by_key("café.owner")
            .unwrap_err()
            .to_string()
            .contains("multiple files normalize"));
        assert!(store
            .apply(WriteOp::ingest(
                Claim::parse_key("café.owner").unwrap(),
                scalar("replace"),
                TriageSource::Ingest,
                Volatility::Days,
                None
            ))
            .is_err());
        assert_eq!(std::fs::read(canonical).unwrap(), before);
        assert!(legacy.is_file());
    }

    #[test]
    fn legacy_ingest_hints_are_disarmed_but_explicit_grounds_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(dir.path()).unwrap();
        for draft in [false, true] {
            let key = if draft { "hint.owner" } else { "bound.owner" };
            ingest(&store, key);
            let mut fact = store.read_fact_by_key(key).unwrap();
            let unchecked = ground_with(None);
            // Reproduce the old representation in the Ingest op and working fact.
            let mut records = store.read_ops().unwrap();
            let ingest = records.last_mut().unwrap();
            if draft {
                fact.grounds.push(unchecked.clone());
            }
            let mut snapshot = serde_json::to_value(&fact).unwrap();
            snapshot.as_object_mut().unwrap().remove("source_hint");
            ingest.changes[0].after = Some(snapshot.to_string());
            let mut log = String::new();
            for record in &records {
                log.push_str(&serde_json::to_string(record).unwrap());
                log.push('\n');
            }
            std::fs::write(store.ops_path(), log).unwrap();
            fact.grounds = vec![ground_with(Some(Outcome::Confirmed))];
            fact.epistemics.groundedness = Groundedness::Verified {
                at: Utc::now(),
                by: GeneratorHash("existence".into()),
            };
            fact.epistemics.confidence = 0.9;
            let mut saved = serde_json::to_value(&fact).unwrap();
            saved.as_object_mut().unwrap().remove("source_hint");
            std::fs::write(store.root().join(fact.rel_path()), saved.to_string()).unwrap();
            let loaded = store.read_fact_by_key(key).unwrap();
            if draft {
                assert!(loaded.grounds.is_empty());
                assert!(loaded.source_hint.is_some());
                assert!(matches!(
                    loaded.epistemics.groundedness,
                    Groundedness::Ungrounded { .. }
                ));
                assert_eq!(loaded.epistemics.confidence, 0.4);
            } else {
                assert_eq!(loaded, fact);
            }
        }
    }

    #[test]
    fn live_lock_is_never_stolen_based_on_its_age() {
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(dir.path()).unwrap();
        let lock = store.lock().unwrap();
        lock.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        assert!(JishukenStore::open(dir.path()).is_err());
        drop(lock);
        JishukenStore::open(dir.path()).unwrap();
    }

    #[test]
    fn op_log_since_keeps_only_newer_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = JishukenStore::init(&dir.path().join(".ken")).unwrap();

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
    fn refuted_grounds_preserve_the_latest_check_identity() {
        let mut latest = ground_with(Some(Outcome::Refuted));
        let at = Utc::now();
        let by = GeneratorHash("refuting-verifier".into());
        let check = latest.last.as_mut().unwrap();
        check.at = at;
        check.by = Some(by.clone());
        let mut older = latest.clone();
        older.last.as_mut().unwrap().at = at - chrono::Duration::seconds(1);
        older.last.as_mut().unwrap().by = Some(GeneratorHash("older-verifier".into()));

        // Unchecked and failed reads contribute no verdict or newer timestamp.
        let fact = fact_with_grounds(vec![
            latest,
            older,
            ground_with(None),
            ground_with(Some(Outcome::Errored)),
            ground_with(Some(Outcome::Inconclusive)),
        ]);
        assert_eq!(
            aggregate_groundedness(&fact),
            Groundedness::Refuted { at, by }
        );
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
