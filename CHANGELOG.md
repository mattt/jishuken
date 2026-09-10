# Changelog

## 0.1.0 (2026-06-09)

First release.
The model is settled (`DESIGN.md`, `THREAT-MODEL.md`)
and the spine described in the README works:
the file-backed store and its op log, the data/control-plane split,
typed ground sources (File, Command, Generator, Handler),
heading/quote/line-range locators, pure predicates,
multi-ground conflicts, Kalman confidence decay,
the value-of-information scheduler with an audit floor,
trust-weighted centrality, self-calibration,
and the MCP data plane (tools, resources, prompts, completion).
Deferred work is listed under Limitations in the README,
starting with tree-sitter locator resolution.

### Does it work?

We evaluated ken against a polyrepo testbed with a live docker stack,
comparing agents with and without ken across repeated trials.
Two findings, stated with their caveats:

- When the truth a fact records was verified in the past
  and is no longer reachable at decision time
  (service stopped, value held only in a now-severed database),
  the ken-equipped agent answered correctly in 3 of 3 trials
  by recalling the verified value;
  the agent without ken managed 1 of 3,
  and an agent told only to "verify before trusting" went 0 for 3,
  because there was nothing left to verify.
  N is small and the intervals overlap,
  so treat the numbers as directional rather than significant.
- When the truth stays reachable, every agent re-derives it
  and correctness ties;
  the ken-equipped agent reached the same answers
  with roughly a third of the runtime probing.

A team of agents also built features against the testbed
while using ken through MCP.
Their feedback drove this release's data-plane fixes:
`ken_search` tokenizes and ranks multi-word queries,
a missed `ken_recall` explains the key form and suggests near keys,
and `ken_conflicts` surfaces facts carrying a refuted ground
that has not yet aggregated to a full conflict.

### Notable mechanics in this release

- Ingest applies the fitted Platt recalibration map
  to LLM-triaged confidence (identity until enough samples),
  closing the self-calibration loop end to end.
- The store is plain files with an append-only op log (`ops.jsonl`):
  every write is one tagged record, and `ken undo` restores the last op's saved bytes.
- `ken serve --install-launch-agent` (macOS) writes the launchd plist,
  loads it with `launchctl bootstrap`, and captures logs;
  other platforms get a clear error pointing at their service manager.
