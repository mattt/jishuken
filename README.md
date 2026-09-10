# Jishuken

`ken` is memory for agents that checks whether what they remember is still true.

An agent can remember who owns a release
long after that person has changed teams.
`ken` keeps the source alongside the claim and checks it periodically.
Each answer includes when it was checked and how much confidence remains.

Use it for information that's expensive to rediscover
or unavailable when needed:
a runbook in another repository, a value behind an API,
something a person told you.
If a fact is one file read away, read the file.
Keep the store small enough that its contents are worth checking.

## Installation

The Cargo package and Rust library are named `jishuken`; the command is `ken`.

Build and install from a checkout:

```sh
git clone https://github.com/mattt/jishuken.git
cd jishuken
cargo install --path jishuken
```

Building requires Rust 1.90 or later;
the repository pins its toolchain in `rust-toolchain.toml`.
Ordinary file sources need no other runtime.
Scripts require Deno; reading a pinned repository revision requires Git or jj.

## Usage

In the project where your agent works, create a store and remember a fact:

```sh
ken init
ken add release.owner ryu --volatility days
ken recall release.owner
```

The key is `entity.relation`; the value here is `ryu`.
Keys are case-sensitive and use [Unicode NFC normalization](https://www.unicode.org/reports/tr31/#Normalization_and_Case),
so composed and decomposed spellings of `é` identify the same fact.

New facts are `ungrounded`.
Their initial confidence is capped,
and a caller cannot supply a verified status.

Suppose a neighboring `wiki` checkout contains `Release.md`,
with an `## Owner` section naming `ryu`.
Add a source to `.ken/ken.toml`:

```toml
[sources.wiki]
repo = "../../wiki"
vcs = "git"
```

Paths in this configuration are relative to `.ken/`,
so `../../wiki` reaches a sibling of the project directory.
Bind that section to the fact and check that it contains the stored value:

```sh
ken ground release.owner \
    --source 'wiki:Release.md#owner' \
    --predicate contains
ken recall release.owner --json
```

`ground` adds the binding and runs it immediately.
The JSON response includes the value, confidence, groundedness,
each ground's last result, and verification and due times.
Use `ken verify release.owner` to check all its grounds again.

Read those fields together:

| Field | Meaning |
| --- | --- |
| `confidence` | The current estimate of how likely the value is to hold, adjusted for age. |
| `groundedness` | Whether the fact has been checked and whether its grounds disagree. |
| `grounds` | The sources, predicates, and recorded outcomes behind that status. |

`verified` means the checked grounds confirm the value.
`refuted` means they reject it; do not rely on that answer.
`conflicted` means some grounds confirm it and others refute it.
Inspect the outcomes and confidence before using the answer.
Verification does not replace a scalar value with a new answer.

To correct a fact, add it again under the same key.
This replaces the value and its ground bindings and resets it to `ungrounded`.
An optional `--ground` hint records a suggested source.
It remains separate from active grounds and is never scheduled.
Use `ken ground` with an explicit predicate before verification can begin.
Hints from earlier stores are identified through the op log;
their existence checks no longer count as evidence, and affected facts need rechecking.

For structured values, pass `--json values.json` instead of a scalar.
A JSON array becomes one set-valued fact, checked as a unit,
with per-element timestamps and confidence.
Give an element its own key when it needs a separate source or check.

Other commands expose the store's history and pending work:

```sh
ken search release             # search by entity, relation, or text
ken why release.owner          # sources and verification history
ken conflicts                  # facts whose grounds disagree
ken stale --limit 10           # facts ranked for rechecking
ken log                        # operation history
ken undo                       # undo the last operation
ken doubt release.owner --reason "The runbook is out of date"
```

`doubt` changes confidence and records the reason.
`undo` reverses one logged operation;
a command such as `ground` can produce more than one operation.
Run `ken --help` or `ken <command> --help` for the full CLI.

## MCP

`ken mcp` runs a Model Context Protocol server over standard input and output.
Register it in your client's MCP configuration:

```json
{
  "mcpServers": {
    "ken": {
      "command": "ken",
      "args": ["mcp", "--store", "/path/to/project/.ken"]
    }
  }
}
```

The server exposes the data plane:

| Tool | Use |
| --- | --- |
| `ken_ingest` | Store a value as ungrounded, optionally with a source hint. |
| `ken_recall` | Read an exact `entity.relation` key with its verification details. |
| `ken_search` | Find facts with a text query or filters. |
| `ken_conflicts` | Read conflicts and facts with refuted grounds. |

Resources are available at `ken://fact/<entity.relation>`,
`ken://why/<entity.relation>`, `ken://conflicts`, and `ken://stale`.
The `remember` and `check-memory` prompts help agents
store facts and consult them.

MCP callers cannot invoke `ground`, `verify`, `doubt`, or `grant`.
Run the scheduler separately to check what agents ingest.
This separation depends on the agent's other permissions;
see [Security](#security).

## Sources and predicates

A *ground* binds a source, a locator, and a predicate to a fact.
A check reads that source, selects a span,
and evaluates the predicate against it.
The source handles I/O.
The predicate is a pure comparison between the claim and the selected text:
the same inputs produce the same result.
Choosing a predicate that tests the intended claim
is the store owner's responsibility.

### Files and locators

File paths default to the project containing `.ken/`.
The `store:` prefix reads within `.ken/`,
and configured names such as `wiki:` select another source root.

| Locator | Selects |
| --- | --- |
| `Release.md` | The whole file |
| `Release.md#owner` | A Markdown section |
| `Release.md?q="ryu"` | A quoted substring |
| `Release.md#L40-58` | A line range |

A heading survives edits elsewhere in a document
more reliably than line numbers.
Each resolved span gets a content hash recorded with the check.
Repository sources read the working file unless you pass `--rev` to `ground`.
A commit ID pins the check to that historical revision;
it won't detect later changes to the file.

Tree-sitter locators (`#ts:`) parse but do not resolve.
Direct URL sources such as `https://example.com/page#section`
are also unsupported.
Use a command or a handler to fetch remote content.

### Predicates

| Predicate | Test |
| --- | --- |
| `exists` | The locator resolves to nonempty text. |
| `equals[:literal]` | The span equals the claim, or the given literal. |
| `contains[:literal]` | The span contains the claim, or the given literal. |
| `matches:<regex>` | The span matches a regular expression. |
| `num:<op>:<n>` | A numeric comparison using `<`, `<=`, `>`, `>=`, or `==`. |
| `ptr:<pointer>[:<predicate>]` | Select a value with a JSON Pointer, then test it. |

### Commands

A command supplies its standard output as the source text.
Allow the program in `.ken/ken.toml` before using it:

```toml
[command]
allow = ["curl"]
```

For an API that returns an `owner` field:

```sh
ken ground release.owner \
    --command 'curl -fsS https://api.internal/release' \
    --predicate 'ptr:/owner:equals'
```

This adds a second ground to the runbook example.
If the API says `zangief` while the runbook still says `ryu`,
the fact becomes conflicted.
`ken why release.owner` shows which check disagreed.

Command arguments are split on whitespace and executed directly.
Shell quoting within the command, pipes, and redirection are not supported.
The allowlist checks the program name only;
commands run with the host user's permissions.

### Generators and handlers

Use a Deno generator when reading requires code.
It receives the claim in `KEN_VALUE` and prints source text to standard output.
Bind it with `ken ground <key> --generator <script> --predicate <test>`.
`ken grant <script> --net <host> --read <path>` grants network and file access
and updates bindings that use that generator name.

A handler provides a reusable reader for a named source.
For example, configure an authenticated knowledge base:

```toml
[sources.kb]
handler = "handlers/kb.ts"
net = ["kb.internal"]
env = ["KB_TOKEN"]
```

Save the module at `.ken/handlers/kb.ts`:

```ts
export default {
  async fetch(reference, env) {
    const response = await fetch(`https://kb.internal/page/${reference}`, {
      headers: { authorization: `Bearer ${env.KB_TOKEN}` },
    });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.text();
  },
};
```

Then `--source 'kb:Release#owner'` uses that handler
with the same locator and predicate syntax as a file.
The handler receives environment variables from its `env` allowlist.
Generator and handler hashes include their source and declared capabilities;
a changed hash makes the check fail until its binding is updated.

## Design

### Confidence

Confidence tends toward `0.5` as a fact ages.
The volatility class sets its half-life:
six hours for `hours`, three days for `days`, and ninety days for `slow`.
`immutable` disables decay, including for ungrounded facts,
so reserve it for values that cannot change.
A fact without a source can still age, but cannot earn a verification result.

A confirmation or refutation updates confidence with a scalar Kalman filter.
As time passes, the estimate's variance grows,
so new evidence has more influence on an older belief.
Commands and network-enabled scripts
have higher measurement noise than local reads;
their results move confidence less.

Checks return `Confirmed`, `Refuted`, `Errored`, or `Inconclusive`.
Only confirmation and refutation update confidence.
A timeout or execution error leaves the previous evidence in place;
confidence continues to age according to the configured volatility.
An unavailable source never counts as a fresh confirmation.

The engine logs prior confidence and check outcomes
to calibrate future LLM priors.
It fits a Platt recalibration map once enough samples exist.
`ken calibration` reports calibration error and reliability bins.
These numbers assess the recorded checks;
they cannot establish that a source or predicate is trustworthy.

### Scheduling

`ken tick` checks facts ranked by expected value:

```text
priority = (1 - current confidence) * consequence / check cost
```

Consequence uses Katz centrality over facts and their entities.
Ungrounded and refuted facts contribute no weight;
verified facts contribute fully, and conflicted facts contribute half.
Generator and handler costs use measured median runtimes when available.

A separate audit budget samples facts with distinct verifier identities.
Identity is a generator or handler hash;
file and command grounds share one identity.
Two file sources alone therefore do not qualify for these audits.
Different hashes also do not prove that two sources are independent.

The scheduler runs once per tick, or continuously as a daemon:

```sh
ken tick
ken serve --interval 5m
```

On macOS, `ken serve --install-launch-agent` installs and loads a launch agent
for the current store.
Use `--uninstall-launch-agent` to remove it.
On other platforms, run `ken serve` under your service manager.

`ken init` writes the defaults to `.ken/ken.toml`.
The scheduling and timeout settings are:

```toml
[budget]
per_tick = 20
audit_per_tick = 5
epsilon = 0.02
concurrency = 1

[volatility]
immutable = "never"
slow = "90d"
days = "3d"
hours = "6h"

[daemon]
interval = "60s"

[sandbox]
runtime = "deno"
timeout = "10s"
default_caps = []
```

`per_tick` limits selected facts, and `audit_per_tick` limits additional audits.
Each selected fact can run several ground checks,
so these are not caps on process count or total running time.
`concurrency` limits simultaneous ground reads;
the resulting writes are applied serially.

### Storage

The store is a `.ken/` directory:

```text
.ken/
  ken.toml
  facts/<entity>/<relation>.json
  ops.jsonl
  verifiers/
  handlers/
```

`ken` finds it by walking up from the current directory.
Use `--store /path/to/.ken` or `KEN_STORE` to select one explicitly.
The store has no embedded version control
and does not change your project's history.

Fact files hold the current state.
Every `WriteOp` produces one tagged record in `ops.jsonl`,
including the before and after bytes of affected files.
Writes use a store lock; operate the store with a single writer.
Changes are staged, then committed with a rollback journal and atomic file replacement.
An interrupted operation is rolled back when the store next opens.
Fact filenames escape punctuation, Unicode, and uppercase letters
to keep distinct keys separate on case-insensitive filesystems.
Existing filenames migrate on the next write.
If existing files have canonically equivalent keys, ken reports the duplicate for resolution.
`undo` restores the last record's saved bytes and removes that record.
It is the operation that rewrites history.

The Rust library exposes `JishukenStore` for embedding,
with reads, `apply(WriteOp)`, conflict inspection, log access, and undo.
Control-plane operations require a token constructed in `verify`.
Library callers are trusted:
the public control APIs are available to code embedding the crate.

## Security

`ken` assumes a trusted store owner who controls the host,
configuration, source bindings, and executable code.
Treat it as a local tool under that owner's account.
`ken` is not suitable for hostile multi-tenant use.

The data plane accepts claims and returns stored information.
The control plane runs checks, changes confidence,
and grants script permissions.
Keeping privileged operations out of MCP prevents an ingested instruction
such as "mark this verified" from directly setting groundedness.
An agent with shell access to `ken verify`, the library's control APIs,
or write access to `.ken/` can cross that boundary.
Restrict those permissions when relying on it.

An MCP client can suggest a file source, but hints do not authorize reads.
Review the source and predicate before binding them with `ken ground`.
File reads use the host filesystem without a sandbox.
Ingest can overwrite an existing key and its grounds;
it has no separate challenge queue protecting the old value.

Deno scripts run with declared network, read, and environment permissions,
without write or subprocess permissions, and with a timeout.
Commands have only a program allowlist and timeout.
Review both before enabling them.
A script allowed to read a secret and contact a host can send that secret there.
The runner does not disable remote imports or provide OS isolation.
Content hashes cover the script and capabilities, not its dependency tree.
Generators and handlers with changed source or capabilities
are rejected before execution.

A successful predicate establishes agreement with the text it read.
A compromised endpoint, a stale runbook,
or a predicate that only tests existence can all produce misleading confirmations.
For consequential facts, bind sources with different failure modes
and inspect disagreements.
The span hash records which content was checked;
it does not authenticate that content or prove freshness.

`ops.jsonl` is a local audit trail, not a tamper-proof record.
Anyone who can edit the store can change its facts and history.
Back up the entire directory and protect it with filesystem permissions.
There are no signed checkpoints or external log anchors.

`ken` also lacks per-source ingest quotas, storage and output limits,
verifier mutation tests, and authenticated source identities.
Budget settings and trust-weighted ranking do not supply those protections.

## Development

The crate is in `jishuken/src/`; end-to-end tests are in `jishuken/tests/e2e.rs`.
Run the repository checks with:

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Install Deno to exercise the script and handler integration tests.

## Name and license

Jishuken (自主研) means self-directed study.
The command is `ken`, also the English word for the range of one's knowledge.

Released under the [MIT License](LICENSE).
