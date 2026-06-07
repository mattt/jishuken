//! The control-plane engine: read a fact's grounds, judge each with its pure
//! predicate (DESIGN §6; README "Sources and predicates"), and apply the
//! resulting `GroundCheck` ops. Reading (File/Command/Generator) lives in
//! `ground::resolve`; judging is a pure `Predicate`. This is the only caller of
//! `verify::authority` in normal operation; the data plane (MCP) never reaches it.

use chrono::Utc;

use crate::error::{Error, Result};
use crate::ground::{read_binding, ReadResult};
use crate::schema::{
    Capabilities, ChangeId, Element, Fact, FactValue, GeneratorHash, GroundBinding, GroundSource,
    Groundedness, Outcome, Resolved,
};
use crate::store::{JjStore, VersionedStore};
use crate::verify::authority;

/// The product of reading and judging one ground: the verdict, the replay
/// state to record, any set-diff update, and the measured read cost (seconds).
type GroundOutcome = (Outcome, Option<Resolved>, Option<Vec<Element>>, Option<f64>);

/// The generator hash that produced a check, if the source is a Generator.
fn ground_by(binding: &GroundBinding) -> Option<GeneratorHash> {
    match &binding.source {
        GroundSource::Generator(g) => Some(g.hash.clone()),
        GroundSource::Handler(h) => Some(h.handler.hash.clone()),
        _ => None,
    }
}

/// Force a ground check on a fact now (`ken verify`): check every ground, apply
/// one `GroundCheck` op per ground, then return the resulting groundedness.
///
/// # Errors
/// Returns an error if the fact is unknown, has no grounds bound, or a store
/// read or write fails.
pub fn verify_fact(store: &JjStore, key: &str) -> Result<Groundedness> {
    verify_fact_inner(store, key, true)
}

/// Check every ground of a fact, optionally recomputing centrality after each
/// `GroundCheck`. Single-shot callers recompute (fresh centrality); the
/// scheduler tick defers it to one recompute after the whole tick (Idea 1).
fn verify_fact_inner(store: &JjStore, key: &str, recompute: bool) -> Result<Groundedness> {
    let fact = store.read_fact_by_key(key)?;
    if fact.grounds.is_empty() {
        return Err(Error::Verifier(format!(
            "fact {key} has no grounds; bind one with `ken ground`"
        )));
    }
    let idxs: Vec<usize> = (0..fact.grounds.len()).collect();
    run_checks(store, &fact, &idxs, recompute)?;
    Ok(store.read_fact_by_key(key)?.epistemics.groundedness)
}

/// Audit a confident fact through its *independent* grounds (DESIGN §7): re-check
/// only the grounds whose verifier differs from the incumbent that last verified
/// the fact, so an audit is a differential check, never the incumbent
/// re-confirming itself. Falls back to all grounds when there is no clear
/// incumbent. Disagreement surfaces as `Conflicted` via `aggregate_groundedness`.
///
/// # Errors
/// Returns an error if the fact is unknown, has no grounds, or a store read or
/// write fails.
pub fn audit_fact(store: &JjStore, key: &str, recompute: bool) -> Result<Groundedness> {
    let fact = store.read_fact_by_key(key)?;
    if fact.grounds.is_empty() {
        return Err(Error::Verifier(format!(
            "fact {key} has no grounds; bind one with `ken ground`"
        )));
    }
    run_checks(store, &fact, &independent_indices(&fact), recompute)?;
    Ok(store.read_fact_by_key(key)?.epistemics.groundedness)
}

/// The ground indices whose verifier differs from the incumbent that last
/// verified the fact (the independent set for an audit). When the fact is not
/// `Verified` there is no incumbent, so all grounds are independent.
fn independent_indices(fact: &Fact) -> Vec<usize> {
    let incumbent = match &fact.epistemics.groundedness {
        Groundedness::Verified { by, .. } => Some(by),
        _ => None,
    };
    let independent: Vec<usize> = fact
        .grounds
        .iter()
        .enumerate()
        .filter(|(_, b)| incumbent.is_none_or(|h| ground_by(b).as_ref() != Some(h)))
        .map(|(i, _)| i)
        .collect();
    if independent.is_empty() {
        (0..fact.grounds.len()).collect()
    } else {
        independent
    }
}

/// Check the given grounds of a fact and apply one `GroundCheck` op each. When
/// `recompute` is false the caller is responsible for a later
/// `recompute_centrality` (the tick coalesces it, Idea 1).
fn run_checks(store: &JjStore, fact: &Fact, idxs: &[usize], recompute: bool) -> Result<()> {
    let id = ChangeId::for_claim(&fact.claim);
    for &idx in idxs {
        let (outcome, resolved, set_update, observed_cost) = check_ground(store, fact, idx)?;
        let by = ground_by(&fact.grounds[idx]);
        let op = authority::ground_check(
            id.clone(),
            idx,
            outcome,
            by,
            resolved,
            set_update,
            observed_cost,
        );
        if recompute {
            store.apply(op)?;
        } else {
            store.apply_no_centrality(op)?;
        }
    }
    Ok(())
}

/// Read one ground and judge it with its predicate. Returns the outcome, the
/// resolved replay state, and (for set facts) the merged element list. No
/// judging code runs in a sandbox; reading may spawn, judging never does.
///
/// # Errors
/// Currently infallible; the `Result` mirrors the `GroundCheck` op it feeds, as
/// source-read failures are reported as an [`Outcome`] rather than an error.
pub fn check_ground(store: &JjStore, fact: &Fact, idx: usize) -> Result<GroundOutcome> {
    let binding = &fact.grounds[idx];
    let now = Utc::now();
    let claim = fact.value.as_env();
    let by = ground_by(binding);

    // Time the read so a generator's measured run time can feed VoI (Idea 2).
    let started = std::time::Instant::now();
    let read = read_binding(store.config(), store.root(), binding, &claim);
    let elapsed = started.elapsed().as_secs_f64();
    // Only a generator/handler check that ran to a usable signal has a cost
    // worth recording; a spawn failure or transient miss does not.
    let observed = |oc: Outcome| (by.is_some() && oc.updates_belief()).then_some(elapsed);

    match read {
        ReadResult::Errored(_) => Ok((Outcome::Errored, None, None, None)),
        ReadResult::Transient => Ok((Outcome::Inconclusive, None, None, None)),
        // The locator no longer resolves: an existence failure (the fact moved).
        // Record a refuting Resolved (empty span hash) so the aggregate sees it.
        ReadResult::Unresolved => {
            let resolved = Resolved {
                rev: String::new(),
                span_hash: String::new(),
                at: now,
                outcome: Outcome::Refuted,
                by: by.clone(),
            };
            Ok((
                Outcome::Refuted,
                Some(resolved),
                None,
                observed(Outcome::Refuted),
            ))
        }
        ReadResult::Resolved(span) => {
            let holds = binding.predicate.eval(&claim, &span.text);
            let outcome = if holds {
                Outcome::Confirmed
            } else {
                Outcome::Refuted
            };
            let set_update = if holds {
                diff_set(&fact.value, &span.text)
            } else {
                None
            };
            let observed_cost = observed(outcome);
            let resolved = Resolved {
                rev: span.rev,
                span_hash: span.span_hash,
                at: now,
                outcome,
                by,
            };
            Ok((outcome, Some(resolved), set_update, observed_cost))
        }
    }
}

/// Diff a returned ground-truth set (the resolved span, a JSON array) against
/// the stored elements (DESIGN §1, §8). Per-element so confirming the list does
/// not smear credit. `None` if the fact is not a set or the span is not a JSON
/// array.
fn diff_set(value: &FactValue, span: &str) -> Option<Vec<Element>> {
    let FactValue::Set { elements } = value else {
        return None;
    };
    let returned: Vec<serde_json::Value> = serde_json::from_str(span.trim()).ok()?;
    let now = Utc::now();
    let mut merged = Vec::with_capacity(returned.len());
    for rv in &returned {
        match elements.iter().find(|e| &e.value == rv) {
            Some(existing) => {
                let mut e = existing.clone();
                e.seen = now;
                e.confidence = Some(bump(e.confidence.unwrap_or(ELEMENT_PRIOR)));
                e.conflict = None;
                merged.push(e);
            }
            None => merged.push(Element {
                value: rv.clone(),
                seen: now,
                confidence: Some(NEW_ELEMENT_CONFIDENCE),
                conflict: None,
            }),
        }
    }
    Some(merged)
}

/// Confidence assumed for a re-appearing set element that carries none yet.
const ELEMENT_PRIOR: f64 = 0.5;
/// Confidence assigned to a set element observed for the first time.
const NEW_ELEMENT_CONFIDENCE: f64 = 0.6;
/// Fraction of the gap to certainty a re-confirmation closes for an element.
const ELEMENT_BUMP: f64 = 0.3;

/// Move an element's confidence a fixed fraction toward certainty on re-confirm.
fn bump(prior: f64) -> f64 {
    (prior + ELEMENT_BUMP * (1.0 - prior)).clamp(0.0, 1.0)
}

/// Bind a ground to a fact (`ken ground`), then check it once. Control-plane
/// only. The CLI builds the binding (source + locator + predicate).
///
/// # Errors
/// Returns an error if the fact is unknown or a store read or write fails.
pub fn ground(store: &JjStore, key: &str, binding: GroundBinding) -> Result<Groundedness> {
    let fact = store.read_fact_by_key(key)?;
    let id = ChangeId::for_claim(&fact.claim);
    store.apply(authority::ground(id.clone(), binding))?;

    let fact = store.read_fact_by_key(key)?;
    let idx = fact.grounds.len() - 1;
    let (outcome, resolved, set_update, observed_cost) = check_ground(store, &fact, idx)?;
    let by = ground_by(&fact.grounds[idx]);
    store.apply(authority::ground_check(
        id,
        idx,
        outcome,
        by,
        resolved,
        set_update,
        observed_cost,
    ))?;
    Ok(store.read_fact_by_key(key)?.epistemics.groundedness)
}

/// Entitle a generator with new capabilities (`ken grant`). Re-registers the
/// source (changing its content hash, DESIGN §6a) and re-points the Generator
/// grounds that use it.
///
/// # Errors
/// Returns an error if the generator source cannot be read or re-registered, or
/// a store read or write fails.
pub fn grant(
    store: &JjStore,
    generator_path: &std::path::Path,
    net: &[String],
    read: &[String],
) -> Result<crate::schema::GeneratorRef> {
    let caps = Capabilities {
        net: net.to_vec(),
        read: read.to_vec(),
        env: vec![],
    };
    let src = crate::ground::generator::GeneratorRegistry::load_src(generator_path, caps)?;
    let new_ref = store.registry().put(&src)?;

    let name = new_ref.name.clone();
    let mut facts = store.all_facts()?;
    for f in &mut facts {
        let mut touched = false;
        for g in &mut f.grounds {
            if let GroundSource::Generator(gr) = &g.source {
                if gr.name == name {
                    g.source = GroundSource::Generator(new_ref.clone());
                    touched = true;
                }
            }
        }
        if touched {
            store.put_fact(f)?;
        }
    }
    store.control_commit(
        "Grant",
        &format!("{} net=[{}] read=[{}]", name, net.join(","), read.join(",")),
    )?;
    Ok(new_ref)
}

/// The result of reading and judging one ground, carried from the (possibly
/// parallel) read phase to the serial apply phase of a tick.
struct Check {
    key: String,
    id: ChangeId,
    idx: usize,
    by: Option<GeneratorHash>,
    outcome: Outcome,
    resolved: Option<Resolved>,
    set_update: Option<Vec<Element>>,
    observed_cost: Option<f64>,
}

/// Read and judge one ground into a [`Check`]. Read-only: no store writes, so it
/// is safe to run on many threads at once.
fn do_check(store: &JjStore, fact: &Fact, idx: usize) -> Result<Check> {
    let (outcome, resolved, set_update, observed_cost) = check_ground(store, fact, idx)?;
    Ok(Check {
        key: fact.claim.key(),
        id: ChangeId::for_claim(&fact.claim),
        idx,
        by: ground_by(&fact.grounds[idx]),
        outcome,
        resolved,
        set_update,
        observed_cost,
    })
}

/// Run the read phase over `work`, on up to `concurrency` threads. Writes are
/// never parallel (jj serializes them); only these read-only checks fan out.
fn run_check_phase(
    store: &JjStore,
    work: &[(&Fact, usize)],
    concurrency: usize,
) -> Result<Vec<Check>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    if concurrency <= 1 || work.len() <= 1 {
        return work
            .iter()
            .map(|&(f, idx)| do_check(store, f, idx))
            .collect();
    }
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<(usize, Result<Check>)>> = Mutex::new(Vec::with_capacity(work.len()));
    let workers = concurrency.min(work.len());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(f, idx)) = work.get(i) else { break };
                let r = do_check(store, f, idx);
                out.lock().expect("check phase mutex").push((i, r));
            });
        }
    });
    let mut collected = out.into_inner().expect("check phase mutex");
    collected.sort_by_key(|(i, _)| *i);
    collected.into_iter().map(|(_, r)| r).collect()
}

/// One scheduler tick (DESIGN §7): check the top `VoI` facts under budget, plus
/// an exploration-floor audit of confident high-consequence facts through their
/// independent grounds. Reads fan out under `budget.concurrency`; the resulting
/// ops are applied serially (jj is the write-serialization point) and centrality
/// is recomputed once for the whole tick (Idea 1). Returns the keys checked and
/// their resulting groundedness.
///
/// # Errors
/// Returns an error if listing the store's facts or a check/apply fails.
pub fn tick(store: &JjStore) -> Result<Vec<(String, Groundedness)>> {
    let now = Utc::now();
    let facts = store.all_facts()?;
    let costs = store.cost_table();
    let cfg = store.config();
    let selected = crate::scheduler::select_tick(&facts, now, cfg, &costs);
    let audits = crate::scheduler::select_audits(&facts, now, cfg, &selected);

    // Build the read work list: VoI facts check all grounds; audit facts check
    // only their independent grounds. `ordered_keys` preserves report order.
    let mut work: Vec<(&Fact, usize)> = Vec::new();
    let mut ordered_keys: Vec<String> = Vec::new();
    for &f in &selected {
        ordered_keys.push(f.claim.key());
        work.extend((0..f.grounds.len()).map(|idx| (f, idx)));
    }
    for &f in &audits {
        ordered_keys.push(f.claim.key());
        work.extend(independent_indices(f).into_iter().map(|idx| (f, idx)));
    }

    // Phase 1: read + judge (possibly parallel). Phase 2: apply serially in a
    // deterministic order so the op log is stable regardless of read scheduling.
    let mut checked = run_check_phase(store, &work, cfg.budget.concurrency)?;
    checked.sort_by(|a, b| a.key.cmp(&b.key).then(a.idx.cmp(&b.idx)));
    for c in checked {
        let op = authority::ground_check(
            c.id,
            c.idx,
            c.outcome,
            c.by,
            c.resolved,
            c.set_update,
            c.observed_cost,
        );
        store.apply_no_centrality(op)?;
    }
    if store.recompute_centrality()? {
        store.control_commit("Centrality", "tick")?;
    }

    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for key in ordered_keys {
        if seen.insert(key.clone()) {
            if let Ok(f) = store.read_fact_by_key(&key) {
                results.push((key, f.epistemics.groundedness));
            }
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[&str]) -> FactValue {
        FactValue::Set {
            elements: values
                .iter()
                .map(|v| Element {
                    value: serde_json::json!(v),
                    seen: Utc::now(),
                    confidence: Some(0.5),
                    conflict: None,
                })
                .collect(),
        }
    }

    #[test]
    fn set_diff_adds_persists_and_removes() {
        let stored = set(&["a", "b"]);
        let merged = diff_set(&stored, "[\"b\", \"c\"]").expect("a json array updates the set");
        let values: Vec<String> = merged
            .iter()
            .map(|e| e.value.as_str().unwrap().to_string())
            .collect();
        assert_eq!(values, vec!["b", "c"]);
        let b = merged
            .iter()
            .find(|e| e.value == serde_json::json!("b"))
            .unwrap();
        assert!(b.confidence.unwrap() > 0.5);
    }

    #[test]
    fn set_diff_ignores_non_array_span() {
        assert!(diff_set(&set(&["a"]), "not json").is_none());
        assert!(diff_set(
            &FactValue::Scalar {
                value: serde_json::json!("x")
            },
            "[\"a\"]"
        )
        .is_none());
    }
}
