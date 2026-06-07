//! The proactive, value-of-information scheduler (DESIGN §7). Rank facts by the
//! expected value of checking them, spend a fixed budget per tick on the top-k,
//! and keep a small exploration floor so a confidently-wrong belief cannot sit
//! undisturbed forever.

use crate::config::Config;
use crate::decay::decayed_confidence;
use crate::schema::{Fact, Timestamp};

/// Cost assigned to a fact with no grounds: effectively un-checkable, so it
/// sinks to the bottom of the ranking.
const NO_GROUND_COST: f64 = 1e6;

/// `voi = p_wrong * centrality / cost` (DESIGN §7). Cost is the cheapest ground
/// check (tier-1/2 are cheap, tier-3 carries the verifier estimate).
pub fn voi_score(f: &Fact, now: Timestamp, cfg: &Config) -> f64 {
    let p_wrong = 1.0 - decayed_confidence(f, now, cfg);
    let consequence = f.schedule.centrality;
    let cost = f.min_check_cost().map_or(NO_GROUND_COST, |c| c.max(1e-6));
    p_wrong * consequence / cost
}

/// Standing audit probability for a high-confidence fact, scaled by
/// consequence so the exploration budget lands where wrong is expensive
/// (DESIGN §7). Routed through an *independent* verifier by the caller.
pub fn audit_probability(f: &Fact, cfg: &Config) -> f64 {
    (cfg.budget.epsilon * f.schedule.centrality).clamp(0.0, 1.0)
}

/// Facts ranked by descending `VoI`.
pub fn rank<'a>(facts: &'a [Fact], now: Timestamp, cfg: &Config) -> Vec<&'a Fact> {
    let mut scored: Vec<(&Fact, f64)> = facts.iter().map(|f| (f, voi_score(f, now, cfg))).collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.into_iter().map(|(f, _)| f).collect()
}

/// The top-`per_tick` facts to check this tick (DESIGN §7). Facts with no
/// grounds are skipped — there is nothing to spend the budget on.
pub fn select_tick<'a>(facts: &'a [Fact], now: Timestamp, cfg: &Config) -> Vec<&'a Fact> {
    rank(facts, now, cfg)
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

    fn fact(key: &str, conf: f64, centrality: f64, cost: f64) -> Fact {
        let mut f = crate::test_support::verified_scalar(key);
        f.epistemics.confidence = conf;
        f.schedule.centrality = centrality;
        // A Generator source so the test's `cost` flows into VoI.
        f.grounds = vec![GroundBinding {
            source: GroundSource::Generator(GeneratorRef {
                hash: GeneratorHash("h".into()),
                caps: Capabilities::default(),
                cost_estimate: cost,
                src_path: "verifiers/h.ts".into(),
                name: "h.ts".into(),
            }),
            locator: Locator::Whole,
            predicate: Predicate::Exists,
            last: None,
        }];
        f
    }

    #[test]
    fn wrong_and_consequential_ranks_first() {
        let cfg = Config::default();
        let now = Utc::now();
        let likely_wrong_central = fact("a.x", 0.1, 10.0, 1.0);
        let confident_central = fact("b.x", 0.99, 10.0, 1.0);
        let wrong_trivial = fact("c.x", 0.1, 0.1, 1.0);
        let facts = vec![confident_central, wrong_trivial, likely_wrong_central];
        let ranked = rank(&facts, now, &cfg);
        assert_eq!(ranked[0].claim.key(), "a.x");
    }

    #[test]
    fn cost_lowers_priority() {
        let cfg = Config::default();
        let now = Utc::now();
        let cheap = fact("cheap.x", 0.2, 5.0, 1.0);
        let pricey = fact("pricey.x", 0.2, 5.0, 50.0);
        assert!(voi_score(&cheap, now, &cfg) > voi_score(&pricey, now, &cfg));
    }

    #[test]
    fn budget_caps_the_tick() {
        let mut cfg = Config::default();
        cfg.budget.per_tick = 2;
        let now = Utc::now();
        let facts: Vec<Fact> = (0..5)
            .map(|i| fact(&format!("e{i}.x"), 0.1, 1.0, 1.0))
            .collect();
        assert_eq!(select_tick(&facts, now, &cfg).len(), 2);
    }

    #[test]
    fn audit_probability_scales_with_centrality() {
        let cfg = Config::default();
        let central = fact("a.x", 0.99, 10.0, 1.0);
        let leaf = fact("b.x", 0.99, 0.1, 1.0);
        assert!(audit_probability(&central, &cfg) > audit_probability(&leaf, &cfg));
    }
}
