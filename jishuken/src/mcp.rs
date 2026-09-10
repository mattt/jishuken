//! `ken mcp`: a data-plane-only MCP server over stdio, built on the official
//! `rmcp` SDK. Agents ingest and recall; they cannot verify, override, or grant,
//! because the interface boundary is the trust boundary. This module never
//! imports `jishuken::verify::authority`, so no control-plane op is reachable here.
//!
//! The surface is shaped to MCP conventions, not just RPC: tools carry
//! read-only/idempotent annotations, reads are also addressable as resources
//! (`ken://fact/...`, `ken://why/...`, `ken://conflicts`, `ken://stale`),
//! prompts ship the "how to remember well" discipline, completion suggests the
//! keys actually in the store, and the server's `instructions` teach the one
//! rule that matters: value without groundedness is a guess.

use std::path::PathBuf;

use jishuken::decay::{decayed_confidence, half_life_secs};
use jishuken::ground::locator::parse_source;
use jishuken::ground::{ground_source_for, render_ground};
use jishuken::predicate::Predicate;
use jishuken::scheduler;
use jishuken::schema::{
    Claim, Fact, FactValue, GroundBinding, Groundedness, Outcome, TriageSource, Volatility,
};
use jishuken::store::JishukenStore;
use jishuken::write::WriteOp;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    AnnotateAble, CallToolResult, CompleteRequestParams, CompleteResult, CompletionInfo, Content,
    GetPromptRequestParams, GetPromptResult, Implementation, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, Prompt,
    PromptArgument, PromptMessage, PromptMessageRole, RawResource, RawResourceTemplate,
    ReadResourceRequestParams, ReadResourceResult, Reference, ResourceContents, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{
    schemars, tool, tool_handler, tool_router, transport::stdio, ErrorData, RoleServer,
    ServerHandler, ServiceExt,
};
use serde_json::{json, Value};

/// What every connecting agent is told about the store before it acts.
/// The epistemics discipline and the trust boundary made legible.
const INSTRUCTIONS: &str = "\
ken is self-verifying memory. Every fact carries two signals you must read before \
you trust it, and they never collapse into one: `confidence` is how much to believe \
it (it decays with time), and `groundedness` is whether it was ever checked against \
a real source. An `ungrounded` fact is an LLM guess or raw ingest, so a high \
confidence on an ungrounded fact is still a guess. `verified` means its checked \
grounds confirm the claim; `refuted` means they reject it. A `conflicted` fact has \
both confirming and refuting grounds. Do not rely on a refuted value. Read the \
ground outcomes and timestamps as well as the status.

This is ken's data plane. You can recall, search, and ingest, and browse facts as \
resources. You cannot verify, ground, override belief, or grant capabilities here, \
by design, so that ingesting an untrusted page can never mark a fact verified. An \
ingested fact always lands `ungrounded`; name where it should be checked with the \
`ground` hint so the scheduler can verify it later. If you confirm against a live \
source that a stored value has changed, re-ingest the corrected value with a `ground` \
hint: it lands `ungrounded`, which raises its re-verification priority for the next \
scheduler tick. That is how you flag a stale fact; you cannot mark it verified yourself.

Recall takes an exact `entity.relation` key; use search for free text. Search \
tokenizes the query, so multi-word queries match. `conflicts` is a single fact whose \
grounds disagree, not a disagreement across two facts.

Resources: `ken://fact/<entity.relation>` is a fact with full epistemics, \
`ken://why/<entity.relation>` is its provenance, `ken://conflicts` lists facts whose \
grounds disagree (and facts with a refuted ground not yet in full conflict), and \
`ken://stale` ranks the facts most worth re-checking.";

#[derive(Clone)]
pub struct JishukenServer {
    store_path: PathBuf,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RecallParams {
    /// `entity.relation`
    key: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct IngestParams {
    /// `entity.relation`
    key: String,
    value: String,
    /// One of: immutable, slow, days, hours.
    volatility: Option<String>,
    meta_confidence: Option<f64>,
    /// Where this should be checked, e.g. `src/auth/verify.rs` or
    /// `wiki:Architecture.md#authentication`. A hint only: the fact stays
    /// `ungrounded` until the control plane checks it.
    ground: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    #[serde(default)]
    query: String,
    entity: Option<String>,
    relation: Option<String>,
    /// ungrounded | verified | refuted | conflicted
    grounded: Option<String>,
}

/// A fact with the epistemics that decide whether to trust it. Structured so a
/// client gets a schema, not a string to re-parse.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RecallOutput {
    /// `entity.relation`.
    key: String,
    /// The stored value: a scalar, or the array of a set-valued fact.
    value: Value,
    /// How much to believe it, in [0,1], after time decay. Not a measure of
    /// whether it was ever checked; read `groundedness` for that.
    confidence: f64,
    groundedness: GroundednessOut,
    /// Independent groundings bound to this fact.
    grounds: Vec<GroundOut>,
    /// When the fact was last confirmed or refuted (RFC 3339).
    last_verified: String,
    /// When it next falls due for a re-check (RFC 3339); absent if it never decays.
    due: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct GroundednessOut {
    /// `ungrounded` (a guess), `verified` (confirmed), `refuted`, or `conflicted`.
    state: String,
    /// The verifier that confirmed or refuted it.
    #[serde(skip_serializing_if = "Option::is_none")]
    verifier: Option<String>,
    /// When it was confirmed or refuted (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<String>,
    /// The disagreeing verifiers, when `conflicted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    verifiers: Option<Vec<String>>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct GroundOut {
    /// The rendered source-and-locator, e.g. `src/auth.rs#L40-58`.
    source: String,
    /// `FILE` | `CMD` | `GEN` | a mount scheme.
    kind: String,
    /// How the span is judged, e.g. `exists`, `equals`, `ptr:/owner:equals`.
    predicate: String,
    /// The outcome of the last check against this ground, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_outcome: Option<String>,
    /// When that check ran (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    last_at: Option<String>,
}

#[tool_router]
impl JishukenServer {
    #[tool(
        description = "Recall a fact: its value plus the full epistemics (confidence and groundedness) that decide whether to trust it.",
        annotations(
            title = "Recall a fact",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn ken_recall(
        &self,
        Parameters(p): Parameters<RecallParams>,
    ) -> Result<Json<RecallOutput>, String> {
        let store = self.open()?;
        recall_output(&store, &p.key)
            .map(Json)
            .map_err(|e| recall_hint(&store, &p.key, &e))
    }

    #[tool(
        description = "Remember a fact. Always lands ungrounded (a belief, not a checked truth). Name where it should be checked with `ground`. Returns the fact id. Re-ingesting an existing key supersedes its value and lands ungrounded again, which raises its re-verification priority for the next scheduler tick: the data-plane way to flag a fact you have seen change.",
        annotations(
            title = "Remember a fact",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn ken_ingest(&self, Parameters(p): Parameters<IngestParams>) -> Result<String, String> {
        let store = self.open()?;
        let claim = Claim::parse_key(&p.key).map_err(|e| e.to_string())?;
        let volatility = p
            .volatility
            .as_deref()
            .and_then(|s| s.parse::<Volatility>().ok())
            .unwrap_or(Volatility::Days);
        let triage = match p.meta_confidence {
            Some(mc) => TriageSource::Llm {
                meta_confidence: mc,
            },
            None => TriageSource::Ingest,
        };
        // `ground` is a hint: a draft binding with the default Exists
        // predicate.
        // Naming a source is not grounding against it; the fact stays
        // ungrounded until the control plane checks it.
        let draft_ground = match p.ground.as_deref() {
            Some(s) => {
                let (src, locator) = parse_source(s, None).map_err(|e| e.to_string())?;
                let source = ground_source_for(store.config(), store.root(), src)
                    .map_err(|e| e.to_string())?;
                Some(GroundBinding {
                    source,
                    locator,
                    predicate: Predicate::Exists,
                    last: None,
                })
            }
            None => None,
        };
        // Data plane only: the single op untrusted callers can construct.
        let op = WriteOp::ingest(
            claim,
            FactValue::Scalar {
                value: Value::String(p.value),
            },
            triage,
            volatility,
            draft_ground,
        );
        let id = store.apply(op).map_err(|e| e.to_string())?;
        Ok(json!({ "id": id.0, "key": p.key, "groundedness": "ungrounded" }).to_string())
    }

    #[tool(
        description = "Find facts by entity, relation, text, or groundedness. The text query is tokenized on whitespace and ranked by how many tokens match key or value, so broad multi-word queries work. Returns each match plus a resource link to its full epistemics.",
        annotations(
            title = "Search facts",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn ken_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, String> {
        let store = self.open()?;
        let now = chrono::Utc::now();
        let tokens = query_tokens(&p.query);
        let facts = store.all_facts().map_err(|e| e.to_string())?;
        let mut ranked: Vec<(usize, &Fact)> = facts
            .iter()
            .filter(|f| p.entity.as_ref().is_none_or(|e| &f.claim.entity.0 == e))
            .filter(|f| p.relation.as_ref().is_none_or(|r| &f.claim.relation == r))
            .filter(|f| {
                p.grounded
                    .as_ref()
                    .is_none_or(|g| f.epistemics.groundedness.label() == g.to_lowercase())
            })
            .filter_map(|f| {
                if tokens.is_empty() {
                    return Some((0usize, f));
                }
                let matched = hit_score(&f.claim.key(), &f.value.render(), &tokens);
                (matched > 0).then_some((matched, f))
            })
            .collect();
        // Rank by how many query tokens a fact matches; filesystem order breaks
        // ties. Tokenizing is what lets a broad query ("free orders webhook")
        // match facts that no contiguous substring would.
        ranked.sort_by(|a, b| b.0.cmp(&a.0));
        let hits: Vec<&Fact> = ranked.into_iter().map(|(_, f)| f).collect();

        let matches: Vec<Value> = hits
            .iter()
            .map(|f| {
                let conf = decayed_confidence(f, now, store.config());
                json!({
                    "key": f.claim.key(),
                    "value": value_of(&f.value),
                    "confidence": round2(conf),
                    "groundedness": f.epistemics.groundedness.label(),
                    "grounds": serde_json::to_value(grounds_out(f)).unwrap_or_default(),
                })
            })
            .collect();

        // The JSON payload for clients that read content, plus a resource link
        // per match so a fact can be fetched or attached on demand.
        let mut content = vec![Content::text(
            serde_json::to_string_pretty(&json!({ "matches": matches })).unwrap_or_default(),
        )];
        for f in &hits {
            let key = f.claim.key();
            content.push(Content::resource_link(
                RawResource::new(format!("ken://fact/{key}"), key.clone())
                    .with_description(format!(
                        "{} ({})",
                        truncate(&f.value.render(), 60),
                        f.epistemics.groundedness.label()
                    ))
                    .with_mime_type("application/json"),
            ));
        }
        Ok(CallToolResult::success(content))
    }

    #[tool(
        description = "List facts whose grounds disagree: `conflicts` (one fact with one ground confirmed and another refuted) and `distrusted` (a fact carrying a refuted ground that has not yet aggregated to full conflict). Disagreements across different facts are not surfaced here.",
        annotations(
            title = "List conflicts",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn ken_conflicts(&self) -> Result<String, String> {
        let store = self.open()?;
        Ok(serde_json::to_string_pretty(&conflicts_value(&store)?).unwrap_or_default())
    }
}

impl JishukenServer {
    fn open(&self) -> Result<JishukenStore, String> {
        JishukenStore::open(&self.store_path).map_err(|e| format!("store: {e}"))
    }

    fn open_data(&self) -> Result<JishukenStore, ErrorData> {
        self.open().map_err(|e| ErrorData::internal_error(e, None))
    }
}

// --- shared read helpers, used by both tools and resources ---

fn recall_output(store: &JishukenStore, key: &str) -> Result<RecallOutput, String> {
    let fact = store.read_fact_by_key(key).map_err(|e| e.to_string())?;
    let now = chrono::Utc::now();
    let conf = decayed_confidence(&fact, now, store.config());
    let due = half_life_secs(fact.schedule.volatility, store.config()).map(|hl| {
        (fact.schedule.last_verified + chrono::Duration::seconds(hl as i64)).to_rfc3339()
    });
    Ok(RecallOutput {
        key: key.to_string(),
        value: value_of(&fact.value),
        confidence: round2(conf),
        groundedness: groundedness_out(&fact.epistemics.groundedness),
        grounds: grounds_out(&fact),
        last_verified: fact.schedule.last_verified.to_rfc3339(),
        due,
    })
}

fn conflicts_value(store: &JishukenStore) -> Result<Value, String> {
    let ids = store.list_conflicts().map_err(|e| e.to_string())?;
    let facts = store.all_facts().map_err(|e| e.to_string())?;
    let mut conflicts = Vec::new();
    let mut distrusted = Vec::new();
    for f in &facts {
        if ids.contains(&f.id) {
            conflicts.push(json!({ "key": f.claim.key(), "value": f.value.render() }));
            continue;
        }
        // A ground was refuted but the fact has not aggregated to `conflicted`
        // (e.g. only one of several grounds has been checked). Surface it so a
        // half-checked disagreement is not invisible to the agent.
        let refuted: Vec<String> = f
            .grounds
            .iter()
            .filter(|g| {
                g.last
                    .as_ref()
                    .is_some_and(|r| matches!(r.outcome, Outcome::Refuted))
            })
            .map(render_ground)
            .collect();
        if !refuted.is_empty() {
            distrusted.push(json!({
                "key": f.claim.key(),
                "value": f.value.render(),
                "refuted_grounds": refuted,
            }));
        }
    }
    Ok(json!({ "conflicts": conflicts, "distrusted": distrusted }))
}

/// Facts ranked by value of information: the ones most likely to be both wrong
/// and consequential.
/// A read-only window onto what the scheduler would check next, so an agent can
/// distrust the memories that are due.
fn stale_value(store: &JishukenStore) -> Result<Value, String> {
    let facts = store.all_facts().map_err(|e| e.to_string())?;
    let now = chrono::Utc::now();
    let costs = store.cost_table();
    let ranked = scheduler::rank(&facts, now, store.config(), &costs);
    let stale: Vec<Value> = ranked
        .into_iter()
        .take(20)
        .map(|f| {
            json!({
                "key": f.claim.key(),
                "voi": round2(scheduler::voi_score(f, now, store.config(), &costs)),
                "confidence": round2(decayed_confidence(f, now, store.config())),
                "groundedness": f.epistemics.groundedness.label(),
            })
        })
        .collect();
    Ok(json!({ "stale": stale }))
}

/// Provenance back to the ground sources (the `ken why` answer, as JSON): where
/// the fact came from, the ground checks against it, and whether they disagree.
fn why_value(store: &JishukenStore, key: &str) -> Result<Value, String> {
    let fact = store.read_fact_by_key(key).map_err(|e| e.to_string())?;
    let mut ops = store.op_log(None).map_err(|e| e.to_string())?;
    ops.reverse();
    let checks: Vec<Value> = ops
        .iter()
        .filter(|op| op.description.contains("[GroundCheck") && op.description.contains(key))
        .map(|op| {
            let outcome = op
                .description
                .split(':')
                .nth(1)
                .and_then(|s| s.split(']').next())
                .unwrap_or("?");
            json!({ "at": op.time, "outcome": outcome, "op": op.id })
        })
        .collect();
    Ok(json!({
        "key": key,
        "value": value_of(&fact.value),
        "ingested_by": fact.provenance.ingested_by,
        "ingested_at": fact.provenance.ingested_at.to_rfc3339(),
        "checks": checks,
        "grounds": serde_json::to_value(grounds_out(&fact)).unwrap_or_default(),
        "conflicted": matches!(fact.epistemics.groundedness, Groundedness::Conflicted { .. }),
    }))
}

fn value_of(v: &FactValue) -> Value {
    match v {
        FactValue::Scalar { value } => value.clone(),
        FactValue::Set { elements } => {
            Value::Array(elements.iter().map(|e| e.value.clone()).collect())
        }
    }
}

fn groundedness_out(g: &Groundedness) -> GroundednessOut {
    match g {
        Groundedness::Ungrounded { .. } => GroundednessOut {
            state: "ungrounded".to_string(),
            verifier: None,
            at: None,
            verifiers: None,
        },
        Groundedness::Verified { at, by } | Groundedness::Refuted { at, by } => GroundednessOut {
            state: g.label().to_string(),
            verifier: Some(by.0.clone()),
            at: Some(at.to_rfc3339()),
            verifiers: None,
        },
        Groundedness::Conflicted { verifiers } => GroundednessOut {
            state: "conflicted".to_string(),
            verifier: None,
            at: None,
            verifiers: Some(verifiers.iter().map(|v| v.0.clone()).collect()),
        },
    }
}

fn grounds_out(fact: &Fact) -> Vec<GroundOut> {
    fact.grounds
        .iter()
        .map(|g| GroundOut {
            source: render_ground(g),
            kind: g.source.kind_label(),
            predicate: g.predicate.label(),
            last_outcome: g.last.as_ref().map(|r| r.outcome.label().to_string()),
            last_at: g.last.as_ref().map(|r| r.at.to_rfc3339()),
        })
        .collect()
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

// --- search and recall-suggestion helpers (pure, unit-tested below) ---

/// Lowercase whitespace tokens of a free-text query.
fn query_tokens(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// How many query tokens appear, as substrings, in a fact's key or value.
fn hit_score(key: &str, value: &str, tokens: &[String]) -> usize {
    let hay = format!("{} {}", key.to_lowercase(), value.to_lowercase());
    tokens.iter().filter(|t| hay.contains(t.as_str())).count()
}

/// How many alphanumeric tokens of `query` appear in a candidate key. Drives
/// the "did you mean" suggestions when a recall misses.
fn key_overlap(candidate_key: &str, query: &str) -> usize {
    let key = candidate_key.to_lowercase();
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .filter(|t| key.contains(t))
        .count()
}

/// The existing keys most similar to `query`, by token overlap, best first.
fn nearest_keys(store: &JishukenStore, query: &str, limit: usize) -> Vec<String> {
    let Ok(facts) = store.all_facts() else {
        return Vec::new();
    };
    let mut ranked: Vec<(usize, String)> = facts
        .iter()
        .map(|f| (key_overlap(&f.claim.key(), query), f.claim.key()))
        .filter(|(overlap, _)| *overlap > 0)
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0));
    ranked.truncate(limit);
    ranked.into_iter().map(|(_, k)| k).collect()
}

/// Turn a bare recall failure into an actionable message: name the likely
/// mistake and point at `ken_search`, with the closest existing keys.
fn recall_hint(store: &JishukenStore, key: &str, err: &str) -> String {
    use std::fmt::Write as _;
    let mut msg = format!("{err}. ");
    if !key.contains('.') {
        msg.push_str(
            "Keys are `entity.relation` (e.g. `staging.url`), not free text; \
             use `ken_search` for text queries. ",
        );
    }
    let suggestions = nearest_keys(store, key, 5);
    if suggestions.is_empty() {
        msg.push_str("No similar keys are in the store.");
    } else {
        let _ = write!(msg, "Did you mean: {}?", suggestions.join(", "));
    }
    msg
}

// --- prompts: the "how to remember well" conventions, shipped as templates ---

const PROMPT_REMEMBER: &str = "remember";
const PROMPT_CHECK_MEMORY: &str = "check-memory";

fn prompt_defs() -> Vec<Prompt> {
    vec![
        Prompt::new(
            PROMPT_REMEMBER,
            Some("Extract durable, verifiable facts worth remembering and ingest them into ken."),
            Some(vec![PromptArgument::new("content")
                .with_description("Text to extract facts from; defaults to the current context.")
                .with_required(false)]),
        ),
        Prompt::new(
            PROMPT_CHECK_MEMORY,
            Some("Recall what ken knows about a topic and weigh it by groundedness before acting."),
            Some(vec![PromptArgument::new("topic")
                .with_description("Entity or subject to recall, e.g. `auth` or `staging.url`.")
                .with_required(false)]),
        ),
    ]
}

fn prompt_messages(
    name: &str,
    arguments: Option<&rmcp::model::JsonObject>,
) -> Option<Vec<PromptMessage>> {
    let arg = |key: &str| {
        arguments
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let text = match name {
        PROMPT_REMEMBER => {
            let content = arg("content")
                .unwrap_or_else(|| "the current conversation and working context".to_string());
            format!(
                "Identify the durable, checkable facts in {content} that an agent would \
                 regret forgetting: things that were true when written but go stale as the \
                 world moves (a file a function lives in, a release owner, a staging URL, a \
                 dependency version). For each, call `ken_ingest` with key `entity.relation`, \
                 a `volatility` of immutable/slow/days/hours, and a `ground` hint naming where \
                 it can be checked (a file path, a command, or a `scheme:reference`). Skip \
                 anything that is a transient detail, an opinion, or belongs in a RAG corpus \
                 rather than as a verifiable claim. Every ingested fact lands ungrounded; the \
                 ground hint is what lets the scheduler verify it later."
            )
        }
        PROMPT_CHECK_MEMORY => {
            let topic = arg("topic").unwrap_or_else(|| "the task at hand".to_string());
            format!(
                "Before acting on {topic}, recall what ken already knows: use `ken_search` to \
                 find related facts, then `ken_recall` (or the `ken://fact/...` resource) for \
                 each one that matters. Weigh every value by its epistemics, not its text: an \
                 ungrounded fact is a guess regardless of confidence, a verified fact was \
                 confirmed against a real source, and a refuted fact was rejected by its \
                 checked grounds. Do not rely on a refuted value. A conflicted fact has \
                 both confirming and refuting grounds; surface that disagreement. Prefer recently verified, \
                 high-confidence facts; treat stale or ungrounded ones as leads to confirm, \
                 not as settled truth. If you confirm against a live source that a stored \
                 value has changed, re-ingest the corrected value with a ground hint so the \
                 scheduler re-verifies it; you cannot mark it verified yourself."
            )
        }
        _ => return None,
    };
    Some(vec![PromptMessage::new_text(PromptMessageRole::User, text)])
}

// --- completion: suggest the keys actually in the store ---

fn completion_values(store: &JishukenStore, arg_name: &str, partial: &str) -> Vec<String> {
    let Ok(facts) = store.all_facts() else {
        return Vec::new();
    };
    let needle = partial.to_lowercase();
    let mut values: Vec<String> = match arg_name {
        "entity" => facts.iter().map(|f| f.claim.entity.0.clone()).collect(),
        "relation" => facts.iter().map(|f| f.claim.relation.clone()).collect(),
        // `key`, `topic`, and anything else complete against full keys.
        _ => facts.iter().map(|f| f.claim.key()).collect(),
    };
    values.retain(|v| v.to_lowercase().contains(&needle));
    values.sort();
    values.dedup();
    values.truncate(CompletionInfo::MAX_VALUES);
    values
}

#[tool_handler]
impl ServerHandler for JishukenServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .enable_completions()
                .build(),
        )
        .with_server_info(Implementation::new("ken", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let store = self.open_data()?;
        let mut resources = vec![
            RawResource::new("ken://conflicts", "conflicts")
                .with_title("Facts in conflict")
                .with_description("Facts currently holding two disagreeing answers.")
                .with_mime_type("application/json")
                .no_annotation(),
            RawResource::new("ken://stale", "stale")
                .with_title("Stale facts")
                .with_description(
                    "Facts ranked by value of information: most likely to be both wrong and consequential.",
                )
                .with_mime_type("application/json")
                .no_annotation(),
        ];
        let facts = store
            .all_facts()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        for f in &facts {
            let key = f.claim.key();
            resources.push(
                RawResource::new(format!("ken://fact/{key}"), key.clone())
                    .with_description(format!(
                        "{} ({})",
                        truncate(&f.value.render(), 60),
                        f.epistemics.groundedness.label()
                    ))
                    .with_mime_type("application/json")
                    .no_annotation(),
            );
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let templates = vec![
            RawResourceTemplate::new("ken://fact/{key}", "fact")
                .with_title("Fact")
                .with_description("A fact (`entity.relation`) with its value and full epistemics.")
                .with_mime_type("application/json")
                .no_annotation(),
            RawResourceTemplate::new("ken://why/{key}", "why")
                .with_title("Provenance")
                .with_description("Where a fact came from and the ground checks against it.")
                .with_mime_type("application/json")
                .no_annotation(),
        ];
        Ok(ListResourceTemplatesResult::with_all_items(templates))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let store = self.open_data()?;
        let uri = request.uri.as_str();
        let body = if uri == "ken://conflicts" {
            conflicts_value(&store)
        } else if uri == "ken://stale" {
            stale_value(&store)
        } else if let Some(key) = uri.strip_prefix("ken://fact/") {
            recall_output(&store, key)
                .and_then(|o| serde_json::to_value(o).map_err(|e| e.to_string()))
        } else if let Some(key) = uri.strip_prefix("ken://why/") {
            why_value(&store, key)
        } else {
            return Err(ErrorData::resource_not_found(
                format!("unknown resource: {uri}"),
                None,
            ));
        };
        let json = body.map_err(|e| ErrorData::internal_error(e, None))?;
        let text = serde_json::to_string_pretty(&json).unwrap_or_default();
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text, uri,
        )
        .with_mime_type("application/json")]))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(prompt_defs()))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        match prompt_messages(&request.name, request.arguments.as_ref()) {
            Some(messages) => Ok(GetPromptResult::new(messages)),
            None => Err(ErrorData::invalid_params(
                format!("unknown prompt: {}", request.name),
                None,
            )),
        }
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        // Only the data-plane references are completable; anything else yields
        // nothing rather than an error.
        let completable = matches!(
            &request.r#ref,
            Reference::Resource(_) | Reference::Prompt(_)
        );
        if !completable {
            return Ok(CompleteResult::default());
        }
        let store = self.open_data()?;
        let values = completion_values(&store, &request.argument.name, &request.argument.value);
        let info = CompletionInfo::with_all_values(values)
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(CompleteResult::new(info))
    }
}

/// Serve the data plane over stdio until the client disconnects.
pub fn serve(store_path: PathBuf) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(async move {
        let service = JishukenServer { store_path }.serve(stdio()).await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{hit_score, key_overlap, query_tokens};
    use super::{IngestParams, JishukenServer, RecallParams, SearchParams};
    use jishuken::store::JishukenStore;
    use rmcp::handler::server::wrapper::Parameters;

    fn ingest_params(key: &str, value: &str) -> IngestParams {
        IngestParams {
            key: key.to_string(),
            value: value.to_string(),
            volatility: None,
            meta_confidence: None,
            ground: None,
        }
    }

    /// Drive the served tool surface end to end against a real store:
    /// ingest -> tokenized search -> recall (hit and actionable miss) ->
    /// conflicts with the `distrusted` section. Control-plane fixture state is
    /// created through `jishuken::engine` (the same path the CLI uses); the served
    /// surface itself stays data-plane only.
    #[test]
    fn tool_surface_roundtrip_against_real_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".ken");
        let store = JishukenStore::init(&root).unwrap();
        let server = JishukenServer {
            store_path: root.clone(),
        };

        // Data plane: ingest three facts through the tool.
        for (key, value) in [
            ("plan.free_orders_limit", "100"),
            ("checkout.v2_enabled", "false"),
            ("auth.handler", "auth lives here"),
        ] {
            let out = server
                .ken_ingest(Parameters(ingest_params(key, value)))
                .unwrap();
            assert!(out.contains("ungrounded"), "ingest lands ungrounded: {out}");
        }

        // Tokenized search: no contiguous substring of any key or value matches
        // this query; token hits must carry it.
        let result = server
            .ken_search(Parameters(SearchParams {
                query: "free orders limit".to_string(),
                entity: None,
                relation: None,
                grounded: None,
            }))
            .unwrap();
        let text = format!("{result:?}");
        assert!(
            text.contains("plan.free_orders_limit"),
            "tokenized search should hit: {text}"
        );

        // Recall hit: exact key returns full epistemics.
        let recalled = server
            .ken_recall(Parameters(RecallParams {
                key: "plan.free_orders_limit".to_string(),
            }))
            .unwrap();
        assert_eq!(recalled.0.key, "plan.free_orders_limit");
        assert_eq!(recalled.0.groundedness.state, "ungrounded");

        // Recall miss with a free-text query: the error must name the mistake,
        // point at ken_search, and suggest the nearest key.
        let Err(err) = server.ken_recall(Parameters(RecallParams {
            key: "free orders limit".to_string(),
        })) else {
            panic!("free-text recall should miss");
        };
        assert!(err.contains("ken_search"), "error should redirect: {err}");
        assert!(
            err.contains("plan.free_orders_limit"),
            "error should suggest the nearest key: {err}"
        );

        // Control-plane fixtures: one fact with a lone refuted ground
        // (distrusted), one with grounds that disagree (conflicted).
        std::fs::write(store.root().join("src.txt"), "auth lives here").unwrap();
        ground(
            &store,
            "checkout.v2_enabled",
            "store:src.txt?q=\"enabled = true\"",
        );
        ground(
            &store,
            "auth.handler",
            "store:src.txt?q=\"auth lives here\"",
        );
        ground(
            &store,
            "auth.handler",
            "store:src.txt?q=\"moved elsewhere\"",
        );

        let recalled = server
            .ken_recall(Parameters(RecallParams {
                key: "checkout.v2_enabled".to_string(),
            }))
            .unwrap();
        assert_eq!(recalled.0.groundedness.state, "refuted");
        assert!(recalled.0.groundedness.at.is_some());
        assert!(recalled.0.groundedness.verifier.is_some());
        assert_eq!(
            recalled.0.grounds[0].last_outcome.as_deref(),
            Some("refuted")
        );

        let result = server
            .ken_search(Parameters(SearchParams {
                query: String::new(),
                entity: None,
                relation: None,
                grounded: Some("refuted".into()),
            }))
            .unwrap();
        let output = serde_json::to_value(result).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(output["content"][0]["text"].as_str().unwrap()).unwrap();
        let hits = payload["matches"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["key"], "checkout.v2_enabled");
        assert_eq!(hits[0]["groundedness"], "refuted");

        let conflicts = server.ken_conflicts().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&conflicts).unwrap();
        let conflicted: Vec<&str> = parsed["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["key"].as_str())
            .collect();
        assert!(
            conflicted.contains(&"auth.handler"),
            "disagreeing grounds should conflict: {conflicts}"
        );
        let distrusted: Vec<&str> = parsed["distrusted"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["key"].as_str())
            .collect();
        assert!(
            distrusted.contains(&"checkout.v2_enabled"),
            "a lone refuted ground should surface as distrusted: {conflicts}"
        );
    }

    fn ground(store: &JishukenStore, key: &str, source: &str) {
        let (src, locator) = jishuken::ground::locator::parse_source(source, None).unwrap();
        let binding = jishuken::schema::GroundBinding {
            source: jishuken::schema::GroundSource::File(src),
            locator,
            predicate: jishuken::predicate::Predicate::Exists,
            last: None,
        };
        jishuken::engine::ground(store, key, binding).unwrap();
    }

    #[test]
    fn query_tokens_lowercases_and_splits() {
        assert_eq!(
            query_tokens("Free Orders Limit"),
            ["free", "orders", "limit"]
        );
        assert!(query_tokens("   ").is_empty());
    }

    #[test]
    fn hit_score_counts_matching_tokens_across_key_and_value() {
        let tokens = query_tokens("free orders webhook");
        // key matches "free" and "orders"; "webhook" matches neither key nor value.
        assert_eq!(hit_score("plan.free_orders_limit", "50", &tokens), 2);
        assert_eq!(
            hit_score("plan.free_orders_limit", "50", &query_tokens("nope")),
            0
        );
    }

    #[test]
    fn hit_score_matches_tokens_in_value() {
        let tokens = query_tokens("postgres");
        assert_eq!(hit_score("db.engine", "Postgres 16", &tokens), 1);
    }

    #[test]
    fn key_overlap_ranks_a_broad_query_above_an_unrelated_key() {
        let q = "free orders limit checkout webhook";
        assert_eq!(key_overlap("plan.free_orders_limit", q), 3);
        assert_eq!(key_overlap("staging.url", q), 0);
    }

    #[test]
    fn key_overlap_ignores_punctuation_in_the_query() {
        // Splits on the dot and underscores: plan, free, orders, limit.
        assert_eq!(
            key_overlap("plan.free_orders_limit", "plan.free_orders_limit"),
            4
        );
    }
}
