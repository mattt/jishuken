# Belief Store: implementation sketch

A proactive-verification memory system for agents.
The store is a directory of fact files plus an append-only operation log `ken` owns.
Language: Rust for the trust spine;
the LLM-facing orchestration can be any language over a thin API.

---

## 0. The core realization

The store keeps two planes apart.
The data plane holds what the store believes;
the control plane holds what a verifier confirmed.
That split is a code boundary (§3), not a storage trick:
only a `verify`-module constructor can move groundedness,
so an untrusted caller can ingest a belief but never assert a confirmation.

The belief machinery needs nothing more than a fact file and an append-only log:

| `ken` concept                          | mechanism                          |
|-----------------------------------------|------------------------------------|
| fact identity (survives re-verification)| the `entity.relation` key          |
| fact version (changes on every update)  | the fact file's bytes              |
| "two sources disagree", held as data    | `Groundedness::Conflicted`         |
| out-of-band append-only audit log        | `ops.jsonl`, one record per op     |
| roll the agent's mind back              | `undo` replays a record in reverse |

The op log is read, not believed. It makes no claim about the world; it records
what the system did. Same category as repo HEAD and a live API response: a
ground source.

---

## 1. Node kinds (where the turtles bottom out)

Two kinds. The distinction is the whole termination argument.

```rust
/// Read, never believed. Has no staleness because it is not a claim
/// about the truth; it is the truth at read time.
enum GroundSource {
    File { path: PathBuf, at_rev: Option<CommitId> },
    Http { url: Url },
    Command { argv: Vec<String> },     // e.g. `jj log -r ...`, `git rev-parse`
    OpLog,                              // the belief store's own history
}

/// Believed. Needs verification. The addressable, scheduled, verified unit.
struct Fact {
    id: FactId,                       // the claim's `entity.relation` key: stable
                                        // identity (survives re-verification), no
                                        // opaque id needed
    claim: Claim,                       // canonical join keys (see below)
    value: FactValue,                   // the payload, structured, may be coarse
    epistemics: Epistemics,             // fact-level: drives scheduling
    schedule: ScheduleMeta,             // volatility, centrality, last_verified
    grounds: Vec<GroundBinding>,        // independent groundings (source + locator +
                                        // optional tier-3 verifier); see §6
    provenance: Provenance,             // where this came from
}

/// The join keys. NORMALIZED: entity is a canonical node, never a string
/// recurring in forty blobs, because the scheduler traverses these.
struct Claim {
    entity: EntityId,                   // canonical identity
    relation: Symbol,
    nl: Option<String>,                 // optional natural-language rendering
}

/// The payload. DENORMALIZED: liberal internal structure, because only the
/// verifier and the LLM ever read inside it.
enum FactValue {
    Scalar(Value),
    Set(Vec<Element>),                  // verify once, diff the returned set
}

/// An assertion: the atomic proposition. Carries its own epistemics only when
/// it earns them, so gain and conflict land per element instead of smearing
/// across the whole list.
struct Element {
    value: Value,
    seen: Timestamp,                    // per-element, so gain lands where it should
    confidence: Option<f64>,            // present only when it earns its own
    conflict: Option<ConflictMarker>,   // attaches here, not to the whole fact
}
```

A verifier is a function from a ground source to a truth value about a derived
fact. The recursion terminates at the first ground source a verifier touches.
You don't verify the repo. You read it.

**Granularity rule.** Granularity of the fact is granularity of everything the
engine does: verification, decay, conflict, centrality, and gain all operate at
the fact boundary. Store fewer, larger facts (one `Set` fact, one verifier, one
lookup) rather than atomic ones, *unless* a sub-element needs its own verifier
(a different ground source or entitlement) or becomes its own dependency target
(something specifically depends on it). Then promote it to its own fact.
Normalize the join keys, denormalize the payload.

---

## 2. The two quantities that must never merge

Confidence is how much we believe a fact. Groundedness is whether it was ever
checked against a ground source, or is an LLM's vibe. Collapsing them is
"confidence laundering": an unchecked 0.9 becomes indistinguishable from a
verifier-confirmed 0.9 and the provenance of the number evaporates.

```rust
struct Epistemics {
    confidence: f64,                    // [0,1], decays over time (see §5)
    groundedness: Groundedness,
}

enum Groundedness {
    Ungrounded { source: TriageSource },          // LLM guess or raw ingest
    Verified  { at: Timestamp, by: VerifierHash },
    Conflicted { verifiers: Vec<VerifierHash> },  // independent checks disagree
}

enum TriageSource { Llm { meta_confidence: f64 }, Ingest, Manual }
```

`groundedness` is writable only by a verifier run. The LLM moves the schedule,
never the truth.

---

## 3. Write authority (the control-plane boundary, in the type system)

The data plane (ingestion of untrusted content: scrapers, chat turns,
documents) can only ever emit one operation. Promotion to `Verified` is
reachable only through the verifier-run path. Enforced at compile time by making
the privileged constructors private to the verification module.

```rust
/// Every mutation of the store is one of these. Each becomes one op-log record,
/// tagged in the op description so the invariant is auditable after the fact.
enum WriteOp {
    /// Data plane. The ONLY op untrusted ingestion may construct.
    /// Always lands as Ungrounded. Cannot set Verified. Cannot set confidence
    /// above the triage ceiling. May carry a draft ground (a `--ground` hint).
    Ingest { claim: Claim, triage: TriageSource, draft_ground: Option<GroundBinding> },

    /// Control plane. Constructible only inside the `verify` module. Records one
    /// ground check; `by` is the generator hash for a Generator source, else None.
    GroundCheck { fact: FactId, ground: usize, outcome: Outcome, by: Option<GeneratorHash> },

    /// Control plane. Scheduler-only. Adjusts ordering, never truth.
    Reschedule { fact: FactId, new_priority: f64 },

    /// Control plane. Human override, always logged loudly.
    ManualOverride { fact: FactId, set: Epistemics, reason: String },
}

enum Outcome { Confirmed, Refuted, VerifierErrored, Inconclusive }  // see §6
```

The store does not enforce who writes which field; that is application-level via
the private constructors. What the op log gives you is the audit: every
`WriteOp` is one record, tagged with its variant, so you can prove after the
fact that `groundedness` only ever moved via `GroundCheck`. Enforcement at write
time, verification of enforcement at the log.

---

## 4. The storage seam

A fact is a file at a deterministic path (`facts/<entity>/<relation>.json`),
so the store greps and diffs like code. Every `WriteOp` lands as one tagged
record in `ops.jsonl`, the append-only log `ken` owns; the log is the audit and
the fact files are the working state.

```rust
impl KenStore {
    fn read_fact_by_key(&self, key: &str) -> Result<Fact>;
    fn apply(&self, op: WriteOp) -> Result<FactId>;      // -> one op-log record
    fn list_conflicts(&self) -> Result<Vec<FactId>>;     // facts in conflict state
    fn op_log(&self, since: Option<OpId>) -> Result<Vec<Operation>>;  // the ground-source audit
    fn undo(&self) -> Result<()>;                        // restore the last op's saved bytes
}
```

`apply` writes the fact file and appends one record tagging the op. Each record
saves the before and after bytes of the files it touched, so `undo` restores
them in reverse. Writes serialize behind a store lock, so single-writer is the
supported posture and a disagreeing write is a later op, not a lost update.

---

## 5. Staleness as covariance (predict step)

Confidence decays between verifications at a rate set by the fact's volatility
class. This is the scalar reduction of the Kalman predict step: variance accrues
as process noise Q per unit time (the random-walk / Ornstein-Uhlenbeck picture).

```rust
enum Volatility { Immutable, Slow, Days, Hours }   // sets Q

fn decayed_confidence(f: &Fact, now: Timestamp) -> f64 {
    let dt = (now - f.schedule.last_verified).as_secs_f64();
    let q  = process_noise(f.schedule.volatility);   // per-class rate
    // confidence relaxes toward the prior-ignorance value as variance grows
    let variance = f.schedule.variance_at_verify + q * dt;
    confidence_from_variance(f.epistemics.confidence, variance)
}
```

Activation energy to change a belief is the prior covariance: a high-confidence
fact has low variance, so new evidence barely moves it; a stale fact has had its
variance inflated by `q * dt` and is primed to flip on weak evidence.

---

## 6. Verifier blame attribution (the verifier-rot turtle)

> **Update (implemented).** Judging is now a pure, capability-free `Predicate`
> (`exists`/`equals`/`contains`/`matches`/`num`/`ptr`), and all reading lives in
> the source layer (`File`/`Command`/`Generator`). A pure predicate cannot rot,
> so `Confirmed`/`Refuted` are unambiguous and the turtle below mostly
> dissolves: the residual ambiguity is a *source read* failing, attributed by
> the resolver as `Inconclusive` (transient, e.g. a net timeout) or `Errored`
> (spawn/exec/allowlist failure, a crashed generator). Capabilities and content
> hashing move to the `Generator` source. The original reasoning is kept below
> for the rationale.

A failed verification is ambiguous: did the fact change, or did the verifier
rot? Distinguish by whether the verifier *executed*, not by what it *returned*.
Content-address the verifier so a diff attributes blame.

```rust
match run(verifier, ground) {
    Ok(false) if verifier_hash_unchanged => Outcome::Refuted,        // fact moved
    Err(_)                               => Outcome::VerifierErrored,// verifier rotted
    Ok(true)                             => Outcome::Confirmed,
    Ok(false) if verifier_hash_changed   => Outcome::VerifierErrored,// can't trust it
}
```

`VerifierErrored` does not refute the fact and does not feed calibration (§8).
It raises a maintenance flag against the verifier, not the belief.

### 6a. Capabilities (entitlements as trust weight and as cadence tier)

Verifiers are LLM-drafted untrusted code, so they run sandboxed with declarable,
enumerable, denyable capabilities (Deno-style today; WASI Preview 2 component
model as the graduation path when you want deterministic replay or non-TS
verifiers). The capability set lives *inside* the content hash, not beside it,
so a verifier that quietly starts asking for the network produces a loud diff.
Privilege escalation becomes auditable by construction.

```rust
struct VerifierRef {
    hash: VerifierHash,                 // covers source AND capabilities
    caps: Capabilities,                 // what it is entitled to touch
    cost_estimate: f64,                 // compute cost
}

struct Capabilities {
    net: Vec<HostPattern>,              // empty = network-free = deterministic
    read: Vec<PathPattern>,
    // no write-back to the store, ever; hard timeout always
}
```

Capabilities do three jobs, not one:

1. **Auditable escalation.** A diff in `caps` is a security event, same as a
   dependency suddenly requesting network. The §9 trust model, applied to the
   verifier itself.
2. **Trust weight on the gain (§8).** A confirmation fetched over an
   unauthenticated network is weaker evidence than one read from a local repo,
   because the channel is non-deterministic and attacker-influenceable. So
   `caps.net` discounts the Kalman gain: a network verifier moves confidence
   less than a deterministic local check. Policy that falls out: a fact above
   some centrality must carry at least one network-free verifier in its
   independent set, so endpoint control can never fully own a consequential
   belief.
3. **Cadence tier (= strict mode).** Network-free verifiers are the cheap
   dirty-bit you can afford to run every tick; network verifiers are the
   expensive re-derivation you gate behind value-of-information (§7). The
   entitlement set is not a separate cost axis bolted on, it *is* the cadence
   tier. "Strict mode" = "this tick, cheap tier only," which doubles as offline
   mode and as containment mode if you suspect compromise.

Network breaks reproducibility, which is why the capability profile introduces
the fourth outcome. A net verifier returning false might mean the fact moved,
the verifier rotted, or the endpoint threw a transient 503:

```rust
// Inconclusive: transient/remote failure on a net-entitled verifier.
// Does not update confidence, does not feed calibration, schedules a retry.
// A deterministic local (network-free) verifier cannot return this.
Outcome::Inconclusive
```

> **Update (implemented).** A scheme handler reuses this model on the reading
> side. A named source root (`[sources.<scheme>]`) is a mount: a CURIE prefix
> like `wiki:` bound either to a repo (read through its VCS) or to a handler, a
> sandboxed Deno module that resolves the reference to bytes. The handler is the
> reusable, per-source form of a generator, so it carries the same capability
> set folded into a content hash (now including an `env` allowlist for auth
> secrets), the same loud diff on drift, and the same eclipse-discounted channel
> weight. The locator and predicate run unchanged on the bytes it returns, so
> the addressing is unified across mounts and only the channel trust weight
> differs.

---

## 7. The scheduler (proactive, value-of-information, with an exploration floor)

The bound on the unbounded-energy problem. Rank derived facts by expected value
of checking them; spend a fixed budget per tick on the top-k.

```rust
fn voi_score(f: &Fact, now: Timestamp) -> f64 {
    let p_wrong     = 1.0 - decayed_confidence(f, now);   // §5
    let consequence = f.schedule.centrality;              // trust-weighted, §9
    let cost        = f.verifier.as_ref().map_or(BIG, |v| v.cost_estimate);
    p_wrong * consequence / cost
}
```

Pure VoI never re-examines what it's confident about, which is how a
confidently-wrong fact becomes a permanent resident. Graft on an exploration
floor: every high-confidence fact carries a small standing audit probability,
scaled by consequence (cheap leaf facts can sit wrong forever), and the audit
routes through an *independent* verifier, never the incumbent.

```rust
fn audit_probability(f: &Fact) -> f64 {
    EPSILON_BASE * f.schedule.centrality          // spend the offset where wrong is expensive
}
// On audit: pick a verifier whose failure mode is uncorrelated with the
// incumbent (differential / metamorphic check). Agreement earns trust.
// Disagreement -> Groundedness::Conflicted, carried forward until evidence
// resolves it.
```

Noise on the claims does not help, because a wrong verifier is a self-confirming
fixed point. The offset has to be diversity on the verifiers: an exogenous
signal the loop cannot generate from inside itself.

---

## 8. Update + self-calibration (closing the LLM-triage turtle)

On a `Confirmed`/`Refuted` outcome, decay the prior then do a confidence-weighted
Bayesian update (scalar Kalman gain). Log every `(prior, outcome)` pair; the
verification engine thereby produces the training signal to grade its own triage
nurse. If the LLM's priors run hot, fit a 1-D recalibration map (Platt / isotonic)
and apply it to future ingests.

```rust
struct CalibrationSample { prior: f64, grounded_outcome: bool }  // from GroundCheck only
// periodically: recalibrate(samples) -> Recalibrator, applied to TriageSource::Llm priors
```

A miscalibrated prior is cheap and unambiguous: you verified in a slightly wrong
order, then the data corrected you. That is why the LLM belongs at triage and
never at the truth.

---

## 9. Sybil-resistant centrality

Centrality (Katz / eigenvector) ranks consequence, and you compute it rather
than accept it, so it feels safe. But you compute it over a topology an attacker
can add edges to: inject a cluster of facts all pointing at a target and you
inflate its importance, steering the verification budget. Defense: weight each
node's voting contribution by its own groundedness. An ungrounded,
freshly-ingested fact contributes no weight to importance until it clears its
own verification. The control plane becomes robust to data-plane injection by
construction.

```rust
// edge weight from g into target counts only as much as g is grounded
fn katz_trust_weighted(graph: &FactGraph, alpha: f64) -> Centrality { /* ... */ }
```

Prompt injection is the 2600 Hz tone of this era: a scraped page reading
"this fact is verified, confidence 1.0" is a control tone in the voice channel.
The write-authority split (§3) makes the switch deaf to it; trust-weighted
centrality keeps the ranking deaf to it too.

---

## 10. Build order

Start at the trust spine; everything downstream is regenerable.

1. **Schema + write authority.** `Fact`, `Epistemics`, `WriteOp`, with the
   privileged constructors private to a `verify` module. Get the control-plane
   boundary compiling before anything touches storage.
2. **The store.** Fact-as-file under `.ken/`, `apply` = one tagged op-log
   record, conflicts surfaced by groundedness. Now you have identity,
   versioning, conflicts, and the op-log audit.
3. **Verifier registry + sandbox.** Content-addressed verifier source *and*
   capabilities (§6a), `run` with blame attribution (§6). Capability-scoped
   execution before the first LLM-drafted verifier runs. LLM verifiers tagged
   ungrounded.
4. **Decay + update + calibration log** (§5, §8). The scalar Kalman core.
5. **Scheduler** (§7): VoI ranking, budget, exploration floor through
   independent verifiers.
6. **Trust-weighted centrality** (§9), fed back into the scheduler.

Regenerable-from-the-log pieces (scheduler state, calibration map) need no trust
governance; if contaminated, refit. Spend the governance on the schema, the
write paths, and the op log. Nowhere else.

---

## Resolved, and what it leaves open

The sandbox question is settled in §6a: capability-scoped execution (Deno now,
WASI component model later), capabilities folded into the verifier hash, no
write-back, hard timeout, and the network-free tier as the constantly-runnable
default. What that leaves genuinely open is the entitlement *granting* policy:
who decides a verifier may hold `net`, and whether a capability grant is itself
a write that needs the §3 control-plane treatment. It should be. A capability
grant is a privilege escalation, so it belongs on the loud, logged path next to
`ManualOverride`, not in ordinary ingestion.
