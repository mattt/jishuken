//! The proactive, value-of-information scheduler.
//! Rank facts by the expected value of checking them, spend a fixed budget per
//! tick on the top-k, and keep a small exploration floor so a confidently-wrong
//! belief cannot sit undisturbed forever.

use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::decay::decayed_confidence;
use crate::schema::{Fact, GeneratorHash, GroundBinding, GroundSource, Timestamp};

/// Cost assigned to a fact with no grounds: effectively un-checkable, so it
/// sinks to the bottom of the ranking.
const NO_GROUND_COST: f64 = 1e6;

/// Measured median run time per generator, keyed by content hash (Idea 2).
/// Empty falls back to each source's static `cost_estimate`.
pub type CostTable = HashMap<GeneratorHash, f64>;

/// The content hash of a binding's source, if it is a generator or handler.
fn ground_hash(b: &GroundBinding) -> Option<&GeneratorHash> {
    match &b.source {
        GroundSource::Generator(g) => Some(&g.hash),
        GroundSource::Handler(h) => Some(&h.handler.hash),
        _ => None,
    }
}

/// A binding's check cost: the measured median when the cost table has it, else
/// the static estimate from the source.
fn binding_cost(b: &GroundBinding, costs: &CostTable) -> f64 {
    ground_hash(b)
        .and_then(|h| costs.get(h))
        .map_or_else(|| b.cost(), |measured| measured.max(1e-6))
}

/// The cheapest ground check for a fact under the current cost table.
fn min_check_cost(f: &Fact, costs: &CostTable) -> Option<f64> {
    f.grounds
        .iter()
        .map(|b| binding_cost(b, costs))
        .min_by(f64::total_cmp)
}

/// `voi = p_wrong * centrality / cost`.
/// Cost is the cheapest ground check (tier-1/2 are cheap, tier-3 carries the
/// measured or estimated cost).
pub fn voi_score(f: &Fact, now: Timestamp, costs: &CostTable) -> f64 {
    let p_wrong = 1.0 - decayed_confidence(f, now);
    let consequence = f.schedule.centrality;
    let cost = min_check_cost(f, costs).map_or(NO_GROUND_COST, |c| c.max(1e-6));
    p_wrong * consequence / cost
}

/// Standing audit probability for a high-confidence fact, scaled by consequence
/// so the exploration budget lands where wrong is expensive.
/// Routed through an *independent* verifier by the caller.
pub fn audit_probability(f: &Fact, cfg: &Config) -> f64 {
    (cfg.budget.epsilon * f.schedule.centrality).clamp(0.0, 1.0)
}

/// A ground's verifier identity: the generator/handler content hash, or a shared
/// `existence` sentinel for verifier-free (file/command) grounds.
fn ground_identity(b: &GroundBinding) -> &str {
    ground_hash(b).map_or("existence", |h| h.0.as_str())
}

/// Whether a fact carries at least two grounds with *distinct* verifier
/// identities.
/// The exploration floor only audits these: re-running a fact's lone verifier
/// is a self-confirming fixed point, so an audit needs an independent verifier
/// to be worth anything.
pub fn has_independent_ground(f: &Fact) -> bool {
    let mut ids: HashSet<&str> = HashSet::new();
    for b in &f.grounds {
        ids.insert(ground_identity(b));
        if ids.len() >= 2 {
            return true;
        }
    }
    false
}

/// A deterministic per-tick draw in `[0,1]` from the fact id and the tick clock,
/// so the audit decision is reproducible and auditable rather than needing an
/// RNG. Folding `now` in means each tick is an independent Bernoulli trial.
fn audit_draw(id: &str, now: Timestamp) -> f64 {
    let mut h = blake3::Hasher::new();
    h.update(id.as_bytes());
    h.update(&now.timestamp_nanos_opt().unwrap_or_default().to_le_bytes());
    let digest = h.finalize();
    let n = u32::from_le_bytes(digest.as_bytes()[..4].try_into().unwrap_or_default());
    f64::from(n) / f64::from(u32::MAX)
}

/// The exploration floor: high-consequence facts the `VoI` ranking would never
/// re-examine, drawn with probability [`audit_probability`] and capped at
/// `budget.audit_per_tick`.
/// Excludes facts already in `selected` and any without an independent ground
/// to audit through.
pub fn select_audits<'a>(
    facts: &'a [Fact],
    now: Timestamp,
    cfg: &Config,
    selected: &[&Fact],
) -> Vec<&'a Fact> {
    let chosen: HashSet<&str> = selected.iter().map(|f| f.id.0.as_str()).collect();
    let mut audits: Vec<&Fact> = facts
        .iter()
        .filter(|f| f.is_checkable() && has_independent_ground(f))
        .filter(|f| !chosen.contains(f.id.0.as_str()))
        .filter(|f| audit_draw(&f.id.0, now) < audit_probability(f, cfg))
        .collect();
    // Spend the cap where wrong is most expensive.
    audits.sort_by(|a, b| audit_probability(b, cfg).total_cmp(&audit_probability(a, cfg)));
    audits.truncate(cfg.budget.audit_per_tick);
    audits
}

/// Facts ranked by descending `VoI`.
pub fn rank<'a>(facts: &'a [Fact], now: Timestamp, costs: &CostTable) -> Vec<&'a Fact> {
    let mut scored: Vec<(&Fact, f64)> = facts
        .iter()
        .map(|f| (f, voi_score(f, now, costs)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.into_iter().map(|(f, _)| f).collect()
}

/// The top-`per_tick` facts to check this tick.
/// Facts with no grounds are skipped — there is nothing to spend the budget on.
pub fn select_tick<'a>(
    facts: &'a [Fact],
    now: Timestamp,
    cfg: &Config,
    costs: &CostTable,
) -> Vec<&'a Fact> {
    rank(facts, now, costs)
        .into_iter()
        .filter(|f| f.is_checkable())
        .take(cfg.budget.per_tick)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::Predicate;
    use crate::schema::*;
    use chrono::Utc;

    fn gen_ground(hash: &str, cost: f64) -> GroundBinding {
        GroundBinding {
            source: GroundSource::Generator(GeneratorRef {
                hash: GeneratorHash(hash.into()),
                caps: Capabilities::default(),
                cost_estimate: cost,
                src_path: format!("verifiers/{hash}.ts"),
                name: format!("{hash}.ts"),
            }),
            locator: Locator::Whole,
            predicate: Predicate::Exists,
            last: None,
        }
    }

    fn fact(key: &str, conf: f64, centrality: f64, cost: f64) -> Fact {
        let mut f = crate::test_support::verified_scalar(key);
        f.epistemics.confidence = conf;
        f.schedule.centrality = centrality;
        // A Generator source so the test's `cost` flows into VoI.
        f.grounds = vec![gen_ground("h", cost)];
        f
    }

    /// A fact with two distinct-identity grounds, so it is audit-eligible.
    fn multi_ground_fact(key: &str, centrality: f64) -> Fact {
        let mut f = fact(key, 0.99, centrality, 1.0);
        f.grounds = vec![gen_ground("h1", 1.0), gen_ground("h2", 1.0)];
        f
    }

    #[test]
    fn wrong_and_consequential_ranks_first() {
        let now = Utc::now();
        let costs = CostTable::new();
        let likely_wrong_central = fact("a.x", 0.1, 10.0, 1.0);
        let confident_central = fact("b.x", 0.99, 10.0, 1.0);
        let wrong_trivial = fact("c.x", 0.1, 0.1, 1.0);
        let facts = vec![confident_central, wrong_trivial, likely_wrong_central];
        let ranked = rank(&facts, now, &costs);
        assert_eq!(ranked[0].claim.key(), "a.x");
    }

    #[test]
    fn cost_lowers_priority() {
        let now = Utc::now();
        let costs = CostTable::new();
        let cheap = fact("cheap.x", 0.2, 5.0, 1.0);
        let pricey = fact("pricey.x", 0.2, 5.0, 50.0);
        assert!(voi_score(&cheap, now, &costs) > voi_score(&pricey, now, &costs));
    }

    #[test]
    fn measured_cost_overrides_static_estimate() {
        let now = Utc::now();
        // Both facts share generator hash "h" with a cheap static estimate; the
        // measured median makes checking it expensive, dropping its VoI.
        let f = fact("a.x", 0.2, 5.0, 1.0);
        let cheap = voi_score(&f, now, &CostTable::new());
        let mut costs = CostTable::new();
        costs.insert(GeneratorHash("h".into()), 100.0);
        let measured = voi_score(&f, now, &costs);
        assert!(measured < cheap, "measured {measured} should be < {cheap}");
    }

    #[test]
    fn budget_caps_the_tick() {
        let mut cfg = Config::default();
        cfg.budget.per_tick = 2;
        let now = Utc::now();
        let costs = CostTable::new();
        let facts: Vec<Fact> = (0..5)
            .map(|i| fact(&format!("e{i}.x"), 0.1, 1.0, 1.0))
            .collect();
        assert_eq!(select_tick(&facts, now, &cfg, &costs).len(), 2);
    }

    #[test]
    fn audit_probability_scales_with_centrality() {
        let cfg = Config::default();
        let central = fact("a.x", 0.99, 10.0, 1.0);
        let leaf = fact("b.x", 0.99, 0.1, 1.0);
        assert!(audit_probability(&central, &cfg) > audit_probability(&leaf, &cfg));
    }

    #[test]
    fn audit_picks_multi_ground_facts_when_forced() {
        let mut cfg = Config::default();
        cfg.budget.epsilon = 1.0; // p = 1, so the draw always selects
        let now = Utc::now();
        let facts = vec![multi_ground_fact("a.x", 1.0)];
        let audits = select_audits(&facts, now, &cfg, &[]);
        assert_eq!(audits.len(), 1);
    }

    #[test]
    fn audit_skips_single_ground_facts() {
        let mut cfg = Config::default();
        cfg.budget.epsilon = 1.0;
        let now = Utc::now();
        // One ground => no independent verifier => never audited.
        let facts = vec![fact("a.x", 0.99, 1.0, 1.0)];
        assert!(select_audits(&facts, now, &cfg, &[]).is_empty());
    }

    #[test]
    fn audit_excludes_already_selected() {
        let mut cfg = Config::default();
        cfg.budget.epsilon = 1.0;
        let now = Utc::now();
        let facts = vec![multi_ground_fact("a.x", 1.0)];
        let selected: Vec<&Fact> = facts.iter().collect();
        assert!(select_audits(&facts, now, &cfg, &selected).is_empty());
    }

    #[test]
    fn audit_zero_epsilon_selects_none() {
        let mut cfg = Config::default();
        cfg.budget.epsilon = 0.0;
        let now = Utc::now();
        let facts = vec![multi_ground_fact("a.x", 1.0)];
        assert!(select_audits(&facts, now, &cfg, &[]).is_empty());
    }

    #[test]
    fn audit_respects_the_cap() {
        let mut cfg = Config::default();
        cfg.budget.epsilon = 1.0;
        cfg.budget.audit_per_tick = 2;
        let now = Utc::now();
        let facts: Vec<Fact> = (0..5)
            .map(|i| multi_ground_fact(&format!("e{i}.x"), 1.0))
            .collect();
        assert_eq!(select_audits(&facts, now, &cfg, &[]).len(), 2);
    }
}
