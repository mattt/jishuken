# Threat Model

This document catalogs the attacks `ken` is designed to withstand and the defenses against each.
It is a design threat model,
not a vulnerability disclosure policy.
Read it alongside `DESIGN.md`,
which carries the rationale for the underlying mechanisms.

## The shape of every attack

Almost every attack on a belief store is after one of three things:
a bounded resource (compute, verification budget, storage),
unearned trust (groundedness, confidence, centrality, priority),
or code execution (the verifier).

Borrowing the vocabulary of distributed systems makes the structure clearer.
The store is an **oracle problem** wearing a **Sybil problem's** clothes.
The ledger was never the hard part.
The hard part is the boundary where external reality enters a system that cannot itself check reality,
and that boundary is the verifier layer.
So the defenses below borrow crypto's threat taxonomy and its economic mechanisms,
while deliberately not borrowing its consensus machinery,
because `ken` has something a public blockchain does not:
a trusted root, namely the store owner and the operation log.
The goal is not to make mutually distrusting strangers agree.
The goal is to make attacks expensive and trust earned.

The unifying defense, stated once:
untrusted input trying to claim unearned influence over a bounded resource,
answered by pricing influence in something an attacker cannot cheaply forge,
which here is verified grounding.
A quota bounds the resource,
a trust-gate withholds influence until grounding is earned,
and an exogenous check catches what the loop cannot catch from inside itself.

## Trust posture is the spine

The catalog of attacks is universal.
The weight of the defenses is not.
Where a deployment sits on the trust spectrum decides which defenses run and how hard.

| Posture    | Deployment                                          | Defenses active                                                                 |
|------------|-----------------------------------------------------|----------------------------------------------------------------------------------|
| High trust | Single org, internal agent fleet, no adversarial ingest | Decay, trust-gate, resource limits, path safety, mutation tests. Economy off.    |
| Mixed      | Org plus semi-trusted partners, some external scraping  | Add source authentication, dispute windows, authenticated identities, checkpointing. |
| Low trust  | Multi-tenant or open ingest from adversaries        | Full economy: reputation, slashing, quorum by centrality, external anchoring.    |

Defaults ship cheap (decay, trust-gate, mutation tests, structural limits) and escalate by posture.
A single-org store should not pay for Sybil resistance it does not need,
and an open store should not be allowed to skip it.

## A. Resource exhaustion

Structural attacks that consume compute, budget, or storage.
The class you harden with quotas.

| Attack            | In the store                                              | Defense                                                              |
|-------------------|----------------------------------------------------------|----------------------------------------------------------------------|
| Cycle             | A depends on B depends on A: invalidation loops, Katz diverges | Dependency graph is a strict DAG; reject cycle-closing edges at write |
| Citation fan-in   | One inferred fact depends on too many others             | In-degree cap                                                        |
| Cascade fan-out   | One ground source a thousand facts depend on             | Out-degree cap                                                       |
| Depth             | Propagation recurses into a stack overflow               | Max dependency depth                                                 |
| Oversized value   | A single fact balloons storage                           | Per-value byte cap (jj's 1 MiB file limit is a backstop, not policy) |
| Flood             | Mass ingest exhausts nodes, edges, budget                | Per-source rate limit; total node and edge budgets                   |

Bound Katz centrality's alpha below the reciprocal of the graph's spectral radius,
with a hard iteration cap,
so the ranking cannot be driven to diverge.

The principle that collapses most of this class:
an ungrounded fact contributes no centrality **and may not create dependency edges** until it clears its own verification.
A structural attack then has to pay verification cost per node before it can distort anything,
which is the same trust-gate used everywhere else, applied to graph structure.

## B. Verifier integrity

The verifier is an oracle.
It asserts that off-system reality matches a stored claim,
and the store cannot independently confirm the oracle told the truth.
Oracle trust cannot be removed, only decentralized, aggregated, and made costly to abuse.

| Attack             | In the store                                                       | Defense                                                                       |
|--------------------|--------------------------------------------------------------------|-------------------------------------------------------------------------------|
| Rubber-stamp oracle| A verifier that is `Deno.exit(0)` confirms everything and looks real | Mutation testing: run against a known-false canary, require `Refuted`         |
| Remote import      | A Deno URL `import` is load-time network that bypasses declared caps | Run with `--no-remote`, vendored dependencies only; treat an import as a capability |
| Sandbox escape     | A V8 or Deno zero-day defeats the language-level sandbox            | Wrap in OS isolation: microVM or gVisor, network namespace, memory cgroup, throwaway uid |
| Exfiltration       | A verifier with `net` to host X encodes secrets into its requests   | Scope caps to exact host and path; a high-centrality fact must also carry a network-free verifier |

Mutation testing is the keystone here.
Nothing else forces a verifier to actually consult its ground source,
so periodically feeding it a value known to be wrong and requiring `Refuted` is the only thing that catches a captured oracle.
A verifier that passes a deliberately false input is quarantined,
its prior outcomes are invalidated,
and only mutation-passing verifiers are allowed to feed the calibration loop,
so a rubber-stamp cannot poison triage globally.
This is the on-chain move of slashing an oracle that reports a known-false value.

For consequential facts, do not trust one oracle.
Confirm with a quorum and aggregate (median for numeric, k-of-n for boolean),
which is the Chainlink lesson rendered in belief terms.

## C. Trust forgery

Claiming influence without earning it.
The Sybil class.

| Attack                 | In the store                                                      | Defense                                                            |
|------------------------|-------------------------------------------------------------------|--------------------------------------------------------------------|
| Sybil on centrality    | An injected cluster of facts inflates a target's importance       | Trust-weighted centrality: an ungrounded fact casts no vote        |
| Volatility laundering  | Injected text marks a fast-changing fact `immutable` so it never re-checks | An ungrounded fact's confidence decays regardless of volatility class |
| Conflict as denial of service | A cheap contradiction deposes a grounded truth into "unknown"  | An ungrounded contradiction is a queued challenge; it does not depose a grounded value |
| Identity spoofing      | An ingest claims to be `trusted-nightly`                          | Identities are authenticated, never self-asserted                  |
| Backdating             | An ingested timestamp makes a fact look freshly verified          | Transaction-time comes only from the op log; ingested times stay ungrounded claims |
| Resolution by counting | A flood of cheap identities wins a vote                           | Weight by evidence authority and stake, never by tally of sources  |

Volatility describes how fast the world changes.
Groundedness describes whether you ever looked.
An unchecked "immutable" fact is still unchecked,
so its confidence bleeds toward "ask again" on a floor that volatility cannot switch off.

One authoritative ground source outweighs a thousand conversational mentions,
and it should,
because the thousand mentions have no stake and the repo has authority.
This is the 51% lesson: never resolve by counting identities.

## D. Grounding and reality (eclipse)

The severe class, because it defeats "read, don't believe" at the root.

| Attack             | In the store                                                                 | Defense                                                                 |
|--------------------|------------------------------------------------------------------------------|-------------------------------------------------------------------------|
| Eclipse            | Control of the path to a ground source (DNS, route, endpoint, package registry) feeds the verifier a false reality | Authenticate the source (TLS pinning, content hashes, signed artifacts); require independent paths for consequential facts |
| Source withholding | A ground source goes permanently unreachable, leaving an unfalsifiable fact   | Persistent unavailability decays toward unknown; silence is decay, not confirmation |
| Replay             | A past `Confirmed` is re-used against a world that has since changed          | Bind every `Confirmed` to the content hash of the ground state it observed, recorded in the op log |

An eclipse is the worst case because the verifier honestly returns `Confirmed` against a source that was swapped underneath it.
Grounding becomes theater.
The defense is to rank a source by how eclipse-able it is:
a content-addressed artifact cannot be eclipsed without breaking its hash,
while plain HTTP against a mutable endpoint can,
so the channel's forgeability discounts the gain,
and a high-centrality fact must carry at least one un-eclipse-able verifier in its quorum.
This is the capability-as-trust-weight rule from `DESIGN.md`, extended to the channel.

Withholding a source is itself an attack,
so the system treats prolonged silence as decay rather than as a license to hold the last-known value forever.
A verification is valid only against the reality it saw,
which is what the replay binding enforces.

## E. Settlement and ordering

Finality is a gradient, and the scheduler is a mempool.

| Attack               | In the store                                                       | Defense                                                              |
|----------------------|--------------------------------------------------------------------|----------------------------------------------------------------------|
| Premature finality   | One confirmation is treated as settled for a high-stakes fact      | Confidence saturates only after N independent confirmations; N scales with centrality |
| Nothing-at-stake     | A source asserts both sides of a contradiction at no cost          | Reputation stake plus slashing on failed verification or equivocation |
| Scheduler MEV        | Inflated staleness or centrality jumps a fact's place in the queue or starves honest verification | Trust-gate priority signals; rate-limit how fast a fact's priority can move |
| Dispute evasion      | A false value rides on fresh "verified recently" status            | A freshly verified high-consequence fact is "confirmed, pending" for a dispute window before it hardens |

Read the confidence scalar the way crypto reads confirmations:
one verification is one confirmation, which is plenty for a low-stakes fact,
and a high-centrality fact should not saturate until several independent confirmations agree.
Slashing turns conflict-as-DoS from free into self-defeating:
injecting a false contradiction from a throwaway source costs that source its stake and does not depose the grounded value,
while a repeat offender bleeds out its reputation.

## F. Audit log integrity

Everything in the design bottoms out at "read the op log, it is the ground source of last resort."
That root must itself be defended.

| Attack             | In the store                                                       | Defense                                                              |
|--------------------|--------------------------------------------------------------------|----------------------------------------------------------------------|
| History rewrite    | `jj op abandon` or `restore` rewrites the audit log                | Hash-chain the op-log heads; sign and anchor them to an external append-only store; access-control the rewriting operations |
| Long-range attack  | An attacker with store access forges a plausible alternate past    | Weak subjectivity: a consumer trusts the latest signed checkpoint, not a re-derivation from genesis |

jj's operation log is append-only in practice but not cryptographically sealed,
so anchoring recent hash-chained heads on a schedule is what makes the long-range attack infeasible.
Before the latest checkpoint, history cannot be plausibly forged.

## G. Input and key safety

The classic, high-severity, unglamorous ones.

| Attack            | In the store                                                       | Defense                                                              |
|-------------------|--------------------------------------------------------------------|----------------------------------------------------------------------|
| Path traversal    | An entity of `../../verifiers/http-200` overwrites a verifier, close to code execution | Hash or strictly encode keys into paths; never interpolate raw input |
| Homoglyph collision | Unicode tricks split one entity into two or collide two into one | Canonicalize entity identity: Unicode normalization plus confusable detection |

## What `ken` does not borrow from crypto

Three places where the analogy would mislead.

`ken` has a trusted root,
so it does not need Byzantine consensus among anonymous parties,
and importing Raft or PBFT-grade machinery would be expensive theater for a store one organization owns.

Verification is probabilistic and noisy where crypto state transitions are deterministic,
so finality here is soft and lives in the confidence scalar rather than in cryptographic settlement.

The economic layer is configurable and off by default,
because a single-org agent fleet wants stake and slashing turned down,
while an open multi-tenant store wants them fully on.

## Configuration

The defense knobs, with conservative defaults, in `ken.toml`:

```toml
[limits]                 # class A
max_in_degree   = 64
max_out_degree  = 512
max_depth       = 16
max_value_bytes = "256KiB"
katz_alpha      = 0.1    # below 1 / spectral_radius
katz_max_iters  = 100
dag_enforce     = true

[rate]
per_source_ingest = "60/min"
max_nodes         = 1_000_000
max_edges         = 4_000_000

[verifier]               # class B
mutation_cadence = "24h" # canary test interval
no_remote        = true
isolation        = "microvm"   # "process" | "gvisor" | "microvm"
timeout          = "10s"
max_memory       = "256MiB"

[economy]                # classes C, E; off in high-trust
enabled          = false
quorum_by_centrality = { low = 1, medium = 2, high = 3 }
dispute_window   = "1h"
reputation       = true
slashing         = true

[audit]                  # class F
checkpoint_cadence = "1h"
anchor_target      = ""  # external append-only log; see below
```

## Hosted transparency log (the commercial seam)

The defense that closes the audit-log and long-range holes is an external, append-only, verifiable record of op-log checkpoints and grounding claims.
For a single org you can self-host it or anchor to any append-only store.
At scale it is a product,
and the structure is the one Certificate Transparency already proved out:
a signed, Merkle-backed log that a third party operates and anyone authorized can audit,
giving an enterprise an independent record that its agents' beliefs were verified against real sources at stated times,
one that survives compromise of the agent host itself.

The reason it monetizes cleanly is the same reason it works as a defense:
its value comes from being operated by a party distinct from the store owner.
Separation is the product.
A store owner who anchors to a log they also control has a convenience feature;
a store owner who anchors to an independent, externally-audited log has a guarantee they can show a regulator.
That is the enterprise tier:
high-trust internally, low-trust externally, and a verifiable record that bridges the two.
