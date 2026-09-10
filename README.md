# Jishuken

Self-verifying memory for agents.
`ken` holds what an agent learned about the systems and people it cannot re-read at decision time,
and tells it how much to still believe each thing.
Facts with a checkable source are re-verified on a budget before they go stale enough to mislead anything;
facts without one decay at a rate set by how fast that kind of thing changes,
so the agent knows when to hedge or ask.

> **Status:** 0.1.0.
> The model is settled (see `DESIGN.md` and `THREAT-MODEL.md`)
> and the spine described in this README works:
> the file-backed store and its op log, the data/control-plane split,
> typed ground sources and locators, multi-ground conflicts,
> the scheduler, self-calibration, and the MCP data plane.
> What is deferred is listed plainly under [Limitations](#limitations),
> starting with tree-sitter locators (`#ts:`), which parse but do not yet resolve;
> use a heading, quoted substring, or line-range locator in the meantime.

## The problem

An agent's memory fills up with facts,
and the facts rot.
The release owner changed at the last reorg,
the on-call rotation moved to a new tool,
the staging URL moved,
a partner bumped their API to v3,
the runbook in the platform team's wiki describes the old deploy,
and the contact who joined Acme three years ago may not be there now.
None of that is in the tree the agent is working in,
so nothing prompts it to look,
and it keeps acting on what was true last quarter.
Keeping all of it current has no upper bound on effort,
so the only sane policy is to spend verification where being wrong is expensive and let the rest decay.

`ken` does two things to make that policy work.
It keeps what it *believes* separate from what it has actually *checked*,
so a confident guess never gets mistaken for a confirmed fact.
And it schedules its own re-verification by expected value,
so the budget goes to the facts most likely to be both wrong and consequential.

## When to use ken

A fact belongs in `ken` when both of these hold:

- Being wrong about it costs something.
- The agent cannot cheaply re-derive it at the moment it acts,
  because the source is in another system, another repo, a live service, or a person.

If the answer is one `grep` or file read away, do not store it.
The agent will re-check it anyway,
and `ken` adds nothing but a second copy that can rot.

If the fact has a source `ken` can read, bind a ground and it is re-verified on a budget.
A fact with no readable source still earns its keep:
`ken` tracks how stale it is,
and the volatility class says how long that kind of fact usually stays true
(the half-life of each class is set in `ken.toml`).
"Joe joined Acme" decays over a few years, because people change jobs;
"M and D got married" barely moves over a decade.
An agent that reads a medium confidence knows to hedge or ask,
and one that reads a high confidence can proceed.
Often a passive ground exists after all:
the last email from `joe@acme.com` confirms "Joe at Acme" as of that date,
and a `Command` source over a mailbox can read it.

`ken` is not a retrieval index.
It tracks a bounded set of facts it can reason about,
not a corpus it searches over.
Point a RAG system at your documents;
point `ken` at the handful of facts whose staleness would actually cost you something.

## Model

There are two kinds of thing in the store.

A **ground source** is read, never believed:
a repo at a revision, a URL, a file, a command, or the store's own operation log.
It has no staleness because it is not a claim about the truth.
It is the truth at read time.

A **derived fact** is believed and needs verification.
Two values ride on it that must never collapse into one.
**Confidence** is how much to believe it,
and it decays over time at a rate set by the fact's volatility.
**Groundedness** is whether it was ever checked against a ground source,
or is still a guess.
A high-confidence guess and a high-confidence verified fact are different objects,
and `ken` will not let you confuse them.

Checking a fact splits into two stages that stay separate.
A fact's **source** is read to a span,
and a **predicate** judges that span against the claim.
The split is the point.
The predicate is pure: the same span always yields the same verdict, so it cannot rot.
Everything that can drift, the file, the endpoint, the network in between, lives on the reading side,
which is also where the cost and the capabilities sit.
A file read is cheap and trusted and runs often;
a command or a networked fetch costs more, counts for less because the channel can lie, and runs on a schedule.

The **scheduler** ranks facts by the expected value of checking them,
roughly the chance a fact is wrong times the cost of acting on it wrong,
divided by what the check costs.
On top of that sits a small floor of random audits,
routed through an independent check rather than the one that last certified the fact,
so a confidently-wrong belief cannot sit undisturbed forever.

### Sources and locators

A ground source is more than a file path.
It is a typed reference to where the truth lives and a *locator* that points at the span within it,
so a fact records not "checked against `Release.md`" but "checked against its `## Owner` section, at wiki revision `9f12`, whose span hashed to `b7c4`."
Pin the revision and you can prove later what the verifier actually saw.
Hash the span and a replay against a since-changed source no longer passes for a fresh confirmation.

The locator is whatever fits the shape of the content, and one fact may carry several:

| locator                    | points at                                       |
|----------------------------|-------------------------------------------------|
| `Doc.md#heading`           | a Markdown section                              |
| `Doc.md?q="…"`             | a quoted substring, re-found if it drifts       |
| `file#L40-58`              | a line range, always bound by the span's hash   |
| `file#ts:(query)`          | a tree-sitter match, for code or any grammar    |
| `https://…#section`        | a fragment of an external page                  |
| `notion:<id>`, `drive:<id>`| a block in a connected system                   |

Line numbers shift the moment someone edits above them,
so a raw range is the weakest anchor and `ken` always pairs it with the span's content hash.
A heading or a tree-sitter query survives edits elsewhere in the file,
which is why they are the preferred anchors for prose and for code respectively.

### The store is files and an op log

`ken` does not reimplement version control.
A fact is a file at `facts/<entity>/<relation>.json`,
so the whole store greps, diffs, and reviews like code,
because it is code-shaped on disk.
Every write lands as one tagged record in an append-only log (`ops.jsonl`):
the log is the audit, the fact files are the working state.

| `ken` concept                  | mechanism                          |
|--------------------------------|------------------------------------|
| fact identity (stable)         | the `entity.relation` key          |
| fact version (per update)      | the fact file's bytes              |
| two answers held at once       | `Groundedness::Conflicted`         |
| the audit log (a ground source)| `ops.jsonl`, one record per write  |
| roll the store back            | `ken undo`                         |

Each op-log record saves the before and after bytes of the files it touched,
so `ken undo` restores them in reverse.
Writes serialize behind a store lock; single-writer is the supported posture.

### Where the store lives

The store is a `.ken/` directory, marked by its `ken.toml`.
`ken` finds it by walking up from the working directory,
the way `git` finds `.git/`,
so each project keeps its own beliefs and the right store is the one you are standing in.
Point somewhere else with `--store` or `KEN_STORE` when you want a shared or global store.

`ken` always operates on the `.ken/` directory explicitly,
never inheriting the ambient repo you are standing in,
so a project's code history and its beliefs stay separate.

## Install

```sh
cargo install ken
ken init        # create a .ken/ store in the current project
```

## CLI

`ken` has two planes.
The **data plane** writes facts that arrive unverified,
and it is the only plane untrusted callers ever touch.
The **control plane** is where truth gets asserted and privileges get granted,
and every command in it is logged loudly.

### Data plane

Add a fact.
It lands `Ungrounded`,
with a confidence the triage step assigns and nothing more.
The key is `entity.relation`;
the rest is the value.
`--ground` names where the truth should be checked,
but naming a source is not grounding against it,
so the fact stays `Ungrounded` until a ground check on the control plane confirms it.

```sh
ken add release.owner "priya" \
    --volatility days \
    --ground 'wiki:Release.md#owner'
```

The runbook lives in the platform team's repo, mounted as `wiki:` (see [Schemes](#schemes)),
so the agent cannot re-read it by grepping its own tree.

Set-valued facts are stored whole and verified in one pass,
with each element tracked on its own so confirming the list does not smear credit over the one entry you were unsure about:

```sh
ken add oncall.roster --json roster.json --volatility days \
    --ground 'wiki:Oncall.md#rotation'
```

Recall a fact.
The human form is a small report;
agents pass `--json`.

```sh
ken recall release.owner
```

```
release.owner = priya
  confidence   0.71   volatility=days
  grounded     verified 6h ago · wiki:Release.md#owner @ git 9f12
  due          in 2d
```

```json
{
  "key": "release.owner",
  "value": "priya",
  "confidence": 0.71,
  "groundedness": { "state": "verified", "at": "2026-06-06T09:14:00Z",
                    "source": "wiki:Release.md#owner @ git 9f12" },
  "last_verified": "2026-06-06T09:14:00Z",
  "due": "2026-06-08T09:14:00Z"
}
```

Every recall carries the epistemics.
A caller that reads `value` and drops the rest has learned the answer without learning whether to trust it,
which is the one thing this store exists to tell it.

### Inspection

```sh
ken why release.owner   # provenance back to the ground sources, with op IDs
ken search release      # find facts by entity, relation, text, or groundedness
ken stale --limit 10    # what is due, ranked by value of information
ken conflicts           # facts currently holding two answers
ken log                 # the operation log (the tagged ops.jsonl records)
ken undo                # roll the store back one operation
```

`ken why` answers the question a stored fact can never answer about itself.
Bind a second, independent ground source to `release.owner` (the live release service, below) and the picture sharpens:
each source shows by name, and a disagreement between them becomes visible rather than silently averaged away.

```
release.owner = priya
  ingested   2026-05-02 by mcp/agent:nightly      (confidence 0.40, ungrounded)
  checked    2026-05-02 wiki:Release.md#owner    -> confirmed  (op 7f3a)
  checked    2026-06-06 wiki:Release.md#owner    -> confirmed  (op c19d)
  ground     WIKI  Release.md#owner  [contains]  @ git 9f12          (net: none)
  checked    2026-06-06 curl api.internal/release -> refuted    (op e8a0)
  ground     CMD   curl -s https://api.internal/release  [ptr:/owner:equals]  (net: api.internal)
  conflict   runbook says priya · service says sam
```

That last line is the runbook-and-service drift made into data.
The runbook ground and the live ground are independent readers of independent systems,
so when they disagree the fact does not silently pick a winner;
it holds both answers and surfaces in `ken conflicts` until something resolves it.
The disagreement is the signal: the runbook is stale, and now you know which way.

### Control plane

```sh
ken ground release.owner \
    --command 'curl -s https://api.internal/release' \
    --predicate 'ptr:/owner:equals'                           # bind an independent, live ground source
ken verify release.owner                                      # force a ground check now
ken doubt  release.owner --reason "..."                       # manual override of belief, logged
ken grant  verifiers/notion-roster.ts --net api.notion.com    # entitle a networked verifier
```

Grounding a fact and granting a capability are both privilege escalations,
binding a belief to reality and widening what a verifier may touch,
so they live here and get logged,
not buried inside `add`.
Data-plane ingest may *name* a source as a hint,
but only the control plane may assert a fact grounded against one.

### Scheduling

`ken` re-checks facts on its own initiative.
A tick ranks facts by value of information and checks the top of the list under a budget;
file reads and their predicates run in-process,
and only a command or generator source spawns a process.

```sh
ken tick                        # run one scheduler pass now
ken serve --interval 5m         # run as a resident daemon, one tick per interval
ken serve --install-launch-agent   # on macOS, register a launch agent for this store
```

## MCP

`ken mcp` exposes the **data plane only**.
Agents ingest and recall.
They cannot ground, verify, override, or grant,
because an agent ingesting a scraped page over the same credential that can mark facts `Verified` is exactly the confidence laundering the design is built to prevent.
The interface boundary is the trust boundary.
The server's `instructions` carry that discipline to the agent:
read groundedness before trusting a value,
and an `Ungrounded` fact is a guess no matter how confident.

| tool            | does                                                       |
|-----------------|------------------------------------------------------------|
| `ken_recall`    | value plus full epistemics (structured output, same shape as `--json`); takes an exact `entity.relation` key, and on a miss points to `ken_search` with the nearest keys |
| `ken_ingest`    | add a fact; always lands `Ungrounded`; takes a `ground` hint; returns id. Re-ingesting a key supersedes its value and raises its re-verification priority |
| `ken_search`    | find facts by entity, relation, or text; the text query is tokenized and ranked by tokens matched, so broad multi-word queries work; links each match to its fact resource |
| `ken_conflicts` | list facts whose grounds disagree (`conflicts`), plus facts carrying a refuted ground not yet in full conflict (`distrusted`) |

The read surface is also addressable as resources,
so a fact can be fetched or attached as context without a tool call:
`ken://fact/<entity.relation>` is a fact with full epistemics,
`ken://why/<entity.relation>` is its provenance,
`ken://conflicts` lists the facts holding two answers,
and `ken://stale` ranks the facts most worth re-checking.
Two prompts ship the conventions:
`remember` extracts durable, verifiable facts from context and ingests them with a ground hint,
and `check-memory` recalls what the store knows and weighs it by groundedness before acting.
Completion suggests the keys actually in the store.
Every surface stays on the data plane; none of it can ground, verify, override, or grant.

Register it like any MCP server:

```json
{ "mcpServers": { "ken": { "command": "ken", "args": ["mcp", "--store", "/path/to/project/.ken"] } } }
```

## Library

`ken` is also a Rust crate,
for embedding in a larger agent runtime.
The store is a `KenStore` reached through a handful of methods:

```rust
impl KenStore {
    fn read_fact_by_key(&self, key: &str) -> Result<Fact>;
    fn apply(&self, op: WriteOp) -> Result<FactId>;   // one op = one op-log record
    fn list_conflicts(&self) -> Result<Vec<FactId>>;
    fn op_log(&self, since: Option<OpId>) -> Result<Vec<Operation>>;
    fn undo(&self) -> Result<()>;
}
```

The constructors for the control-plane variants of `WriteOp` are private to the `verify` module,
so "only a ground check may set groundedness" is a compile-time fact,
not a convention.
See `DESIGN.md` for the schema and the rationale.

## Sources and predicates

Checking a fact splits cleanly into two stages: *reading* a source to a span, and *judging* that span.
Reading is where code and capabilities live; judging is always pure.

A **predicate** is the judge.
It is declarative and capability-free, evaluated over the claim and the resolved span, and it cannot rot:
the same span always gives the same verdict, so a refute is unambiguous, the span moved.

```
exists              the locator resolved to something
equals[:literal]    span equals the claim (or a literal)
contains[:literal]  span contains the claim (or a literal)
matches:<regex>     span matches a regular expression
num:<op>:<n>         span parses as a number and compares (< <= > >= ==)
ptr:<pointer>[:<sub>]  resolve an RFC 6901 JSON Pointer, then judge with <sub>
```

A **source** is the reader, in one of three kinds:

- `File` reads a path at a revision (the default, declarative, every-tick cheap).
- `Command` runs an allowlisted host program and reads its stdout. HTTP folds in here as `curl`. Only programs in `[command] allow` may run.
- `Generator` runs a sandboxed Deno script that emits the value on stdout, for the authenticate-fetch-normalize case. Capabilities (`net`, `read`, `env`) are declared and folded into its content hash, so a generator that starts asking for the network shows up as a diff and a `ken grant`. No write-back to the store; hard timeout.

The kinds form a hierarchy, cheapest and most trusted first.
The same ground can often be expressed in any of them:
reading `config.toml` is a `File`, or `cat config.toml` as a `Command`, or a generator that calls `Deno.readTextFile`.
Always use the simplest kind that can express the ground.
A `File` is deterministic and needs no allowlist or sandbox;
a `Command` earns its allowlist entry only when a plain read cannot reach the value;
a `Generator` is the last resort, for grounds that genuinely have to authenticate, page, or normalize.
Dropping to a more powerful kind costs you determinism, trust, and cadence, so do it only when the kind above cannot do the job.

So "fetch JSON, check a key" needs no code: a `curl` command plus a JSON-pointer predicate.

```sh
ken ground release.owner \
    --command 'curl -s https://api.internal/release' \
    --predicate 'ptr:/owner:equals'
```

Code is reserved for genuine reading work. A generator authenticates, fetches, and normalizes, then prints the ground value:

```ts
// verifiers/notion-roster.ts   caps: { net: ["api.notion.com"] }
const pageId = Deno.env.get("KEN_VALUE")!;
const body = await (await fetch(`https://api.notion.com/v1/blocks/${pageId}/children`)).json();
console.log(JSON.stringify({ owner: body.owner ?? null }));   // a pure predicate judges this
```

A `File` read is deterministic and counts full;
a `Command` or a net-capable `Generator` is more eclipse-able, so it discounts the Kalman gain and is gated behind value of information,
which makes "files only" your offline mode and your containment mode at once.

### Schemes

A named source root is a mount.
`[sources.wiki]` binds the `wiki:` prefix to a place ken reads bytes from,
addressed by a CURIE (a compact URI): a `scheme:reference` prefix, here `wiki:Release`.
A repo-backed mount points at a separate repository (`repo = "../wiki"`), read through its own VCS.
A handler-backed mount points at sandboxed code (`handler = "handlers/wiki.ts"`),
a reusable reader for sources that have to authenticate, page, or normalize.

A handler is the authenticate-fetch-normalize work written once for a whole source instead of once per fact.
It receives the reference (the CURIE's path) and returns the document;
ken then applies the locator and the predicate exactly as it would for a file.

```ts
// handlers/wiki.ts   caps: { net: ["wiki.internal"], env: ["WIKI_TOKEN"] }
export default {
  async fetch(reference, env) {
    const res = await fetch(`https://wiki.internal/page/${reference}`, {
      headers: { authorization: `Bearer ${env.WIKI_TOKEN}` },
    });
    if (!res.ok) throw new Error(`wiki ${reference}: ${res.status}`);
    return await res.text();   // ken slices the #owner section, then judges it
  },
};
```

So `ken ground release.owner --source wiki:Release#owner` reads the wiki through the handler
and judges the `#owner` section, the same locator it would use on a local file.
The symmetry is the addressing and the mount table; the difference is the channel.
A repo mount is a deterministic file read and counts full.
A handler mount is code, so it counts at the generator's tier:
its source and capabilities fold into one content hash, a change in either is a loud diff and a re-ground,
and a net handler discounts the gain like any non-deterministic channel.

## Implementation Details

The qualitative model has a small amount of standard math under it.
Each fact carries a scalar belief that is filtered like a noisy measurement,
ranked for attention by expected value,
weighted for consequence by a centrality that resists injection,
and used to grade the triage that produced it.

### Confidence is a one-dimensional Kalman filter

A fact's confidence $c \in [0,1]$ is a state estimate with a variance $v$,
and verification is the measurement that corrects it.

Between checks the estimate is predicted forward.
Confidence relaxes toward maximal ignorance $c_\infty = 0.5$ at the fact's volatility half-life $h$,
and its variance grows by process noise $Q$ per unit time:

$$
c(\Delta t) = c_\infty + (c_0 - c_\infty)\,2^{-\Delta t / h},
\qquad
v(\Delta t) = v_0 + Q\,\Delta t,
\qquad
Q = \frac{\ln 2}{h}.
$$

The `immutable` class sets $Q = 0$ and never decays;
`hours`, `days`, and `slow` set successively longer half-lives.

A ground check is a measurement $z$, with $z = 1$ for a confirmation and $z = 0$ for a refutation.
The correction is the scalar Kalman step:

$$
K = \frac{v}{v + R},
\qquad
c \leftarrow c + K\,(z - c),
\qquad
v \leftarrow (1 - K)\,v.
$$

The gain $K$ carries the intuition.
A fresh, confident fact has low variance, so $K$ is small and new evidence barely moves it;
a stale fact has had its variance inflated by $Q\,\Delta t$, so $K$ is large and it flips on weak evidence.
The measurement noise $R$ says how much to trust the channel:
a deterministic local read uses $R = 0.05$,
and a non-deterministic one (a command, or a net-capable generator or handler) uses $R = 0.30$,
so the same confirmation moves confidence less when it arrived over a channel that can lie.
That is the "counts for less" from the model, as a number.

### Scheduling by value of information

A tick ranks every checkable fact by the expected value of checking it,
the probability it is wrong times its consequence, divided by what the check costs:

$$
\mathrm{VoI}(f) = \frac{\big(1 - c(\Delta t)\big)\,\kappa_f}{\mathrm{cost}_f},
$$

where $\kappa_f$ is the fact's centrality (below) and $c(\Delta t)$ is its decayed confidence.
The top `per_tick` facts are checked.
Cost is measured rather than guessed:
each generator's run times are folded into a t-digest, and the scheduler uses its median,
falling back to a static estimate until there are samples.

Pure value of information never revisits what it is already sure of,
so a small exploration floor sits on top.
Each confident fact is audited with probability

$$
p_{\text{audit}}(f) = \min\!\big(1,\ \varepsilon\,\kappa_f\big),
$$

drawn deterministically from the fact id and the tick clock,
and routed through an independent ground rather than the one that last certified it,
so a wrong verifier cannot keep confirming itself.

### Consequence is trust-weighted Katz centrality

Consequence $\kappa$ is Katz centrality over the graph of facts and the entities they touch:

$$
\kappa = \beta\,\mathbf{1} + \alpha\,A^{\top} W \kappa,
$$

solved by power iteration with $\alpha = 0.5$ and $\beta = 1$.
The difference from textbook Katz is the diagonal weight matrix $W$.
Each node contributes to importance only in proportion to its own groundedness:
$w_i = 0$ for an ungrounded fact, $0.5$ for one in conflict, and $1$ for a verified one.
An attacker who injects a cluster of facts pointing at a target to inflate its budget gains nothing,
because the injected, unverified nodes carry weight $0$ until they clear their own verification.
The control plane stays robust to data-plane injection by construction.

### Self-calibration of the triage

Every confirmed or refuted check logs the pair $(p, z)$:
the prior confidence the fact carried, and whether it held up.
Those pairs grade the LLM triage that assigned the priors.
When the priors run hot, a one-dimensional Platt map is fit by gradient descent on log-loss
and applied to future ingests:

$$
r(p) = \sigma\!\big(4a\,(p - 0.5) + b\big),
\qquad
\sigma(x) = \frac{1}{1 + e^{-x}}.
$$

The map is the identity until a handful of samples exist, and it is regenerable:
if the calibration log is lost or poisoned, refit it.
A miscalibrated prior is cheap, because it changes only the order of verification, never a fact's groundedness.

## Configuration

`ken.toml`, in the store root:

```toml
[budget]
per_tick = 20          # max verifier runs per scheduler tick
epsilon  = 0.02        # base audit rate, scaled up by a fact's centrality

[volatility]           # confidence half-life per class
immutable = "never"
slow      = "90d"
days      = "3d"
hours     = "6h"

[sandbox]              # governs Generator and handler sources
runtime      = "deno"
timeout      = "10s"
default_caps = []      # network-free unless explicitly granted

[command]              # allowlist for Command sources (argv[0]); empty = none
allow = ["curl", "jq", "git"]

[sources.wiki]         # a mount: the `wiki:` prefix, a separate git repo
repo = "../wiki"       # repo-backed: read through its own VCS
vcs  = "git"

[sources.kb]           # handler-backed: sandboxed code resolves the reference
handler = "handlers/kb.ts"
net = ["kb.internal"]
env = ["KB_TOKEN"]     # host vars the handler may read, folded into its hash
```

A repo-backed mount reads through its VCS:
a jj source via `jj file show` and a git source via `git show <rev>:<path>`,
pinned per fact by `--rev`.
The `.ken/` store itself is plain files, so a `store:` source reads from disk.
A handler-backed mount runs its code in the sandbox instead and reads stdout.
The reader differs; the locator grammar does not.

## Storage layout

```
my-project/
  .jj/                          the project's own code history
  .ken/                         the belief store: plain files, no embedded VCS
    ken.toml                    config and the store marker
    facts/<entity>/<relation>.json   value, epistemics, ground locators, last-seen span hash
    ops.jsonl                   the append-only op log (the audit, one record per write)
    verifiers/<hash>.ts
    handlers/<scheme>.ts             reusable scheme handlers (e.g. handlers/wiki.ts)
../wiki/                        a separate git repo, read as a ground source, never written
  .git/
  Architecture.md
```

A fact carries its ground locators and the span hash each one last resolved to;
the sources themselves stay where they live and `ken` only ever reads them.

## Limitations

What 0.1.0 defers, stated plainly so nothing reads as silently missing.

- Tree-sitter locators (`#ts:`) parse but do not resolve.
  A ground bound to one reads as `Errored` until 0.2.0.
  Use a heading, quoted substring, or line-range locator in the meantime;
  the line-range form is always paired with the span's content hash.
- Networked locators are deferred:
  `https://…#section` does not resolve yet.
  Reach external sources through a `Command` (`curl` plus a predicate)
  or a handler-backed mount instead.
- Runtime dependencies are external.
  `ken` shells out to `deno` for generator and handler sources,
  and to `jj` or `git` for repo-backed mounts read at a pinned revision.
  None are bundled, and the store itself needs neither.
- The launch agent is macOS only.
  `ken serve --install-launch-agent` writes and loads a launchd agent;
  on other platforms, run `ken serve` under your own service manager.

## Non-goals

`ken` is not a general knowledge graph or a query endpoint.
It normalizes the keys it has to traverse (entities and dependency edges) and leaves everything else as loose payload,
so do not expect SPARQL.

It is not a test runner.
A check over fixed inputs that should always pass is a test, and belongs in your CI.
`ken` is for claims that were true when written and go stale because the world moves underneath them:
a staging URL, a release owner, the file a function lives in.
The judge is a pure predicate for that reason.
If it ever fails the same way every time, that is a bug in the check, caught once,
not a fact to re-verify forever.
The drift `ken` watches for is in the source, never in the judgment.

It is not a truth oracle.
`ken` reports how much it currently believes a thing and when it last looked.
Deciding what to do about a stale, low-confidence answer is still your job.

## The name

Jishuken (自主研) is the Toyota practice of *jishu kenkyū*,
self-directed study:
a team goes to the actual workplace and examines how the work really happens rather than trusting the report of it.
The fit is exact.
The system studies its own knowledge on its own initiative (*jishu*, autonomous),
it checks claims against the real source instead of a stored summary (the gemba discipline, our ground sources),
and *ken* (研, and the English "ken") is the range of what is known.
The command is `ken`.
The corpus it keeps is your lore.
