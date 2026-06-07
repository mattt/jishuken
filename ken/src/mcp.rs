//! `ken mcp`: a data-plane-only MCP server over stdio, built on the official
//! `rmcp` SDK. Agents ingest and recall; they cannot verify, override, or grant,
//! because the interface boundary is the trust boundary. This module never
//! imports `ken::verify::authority`, so no control-plane op is reachable here.
//!
//! The surface is shaped to MCP conventions, not just RPC: tools carry
//! read-only/idempotent annotations, reads are also addressable as resources
//! (`ken://fact/...`, `ken://why/...`, `ken://conflicts`, `ken://stale`),
//! prompts ship the "how to remember well" discipline, completion suggests the
//! keys actually in the store, and the server's `instructions` teach the one
//! rule that matters: value without groundedness is a guess.

use std::path::PathBuf;

use ken::decay::{decayed_confidence, half_life_secs};
use ken::ground::locator::parse_source;
use ken::ground::{ground_source_for, render_ground};
use ken::predicate::Predicate;
use ken::scheduler;
use ken::schema::{Claim, Fact, FactValue, GroundBinding, Groundedness, TriageSource, Volatility};
use ken::store::{JjStore, VersionedStore};
use ken::write::WriteOp;
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

/// What every connecting agent is told about the store before it acts. The
/// epistemics discipline (DESIGN §2) and the trust boundary (§3) made legible.
const INSTRUCTIONS: &str = "\
ken is self-verifying memory. Every fact carries two signals you must read before \
you trust it, and they never collapse into one: `confidence` is how much to believe \
it (it decays with time), and `groundedness` is whether it was ever checked against \
a real source. An `ungrounded` fact is an LLM guess or raw ingest, so a high \
confidence on an ungrounded fact is still a guess, not a checked truth. Only \
`verified` facts were confirmed against a ground source; `conflicted` facts hold two \
answers that disagree. Treat the value alone as unreliable until you have read its \
groundedness.

This is ken's data plane. You can recall, search, and ingest, and browse facts as \
resources. You cannot verify, ground, override belief, or grant capabilities here, \
by design, so that ingesting an untrusted page can never mark a fact verified. An \
ingested fact always lands `ungrounded`; name where it should be checked with the \
`ground` hint so the scheduler can verify it later.

Resources: `ken://fact/<entity.relation>` is a fact with full epistemics, \
`ken://why/<entity.relation>` is its provenance, `ken://conflicts` lists facts \
holding two answers, and `ken://stale` ranks the facts most worth re-checking.";

#[derive(Clone)]
pub struct KenServer {
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
    /// ungrounded | verified | conflicted
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
    /// When the fact was last verified (RFC 3339).
    last_verified: String,
    /// When it next falls due for a re-check (RFC 3339); absent if it never decays.
    due: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct GroundednessOut {
    /// `ungrounded` (never checked, treat as a guess), `verified`, or `conflicted`.
    state: String,
    /// The verifier that confirmed it, when `verified`.
    #[serde(skip_serializing_if = "Option::is_none")]
    verifier: Option<String>,
    /// When it was verified (RFC 3339).
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
impl KenServer {
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
        recall_output(&store, &p.key).map(Json)
    }

    #[tool(
        description = "Remember a fact. Always lands ungrounded (a belief, not a checked truth). Name where it should be checked with `ground`. Returns the fact id.",
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
        // `ground` is a hint: a draft binding with the default Exists predicate.
        // Naming a source is not grounding against it (DESIGN §3); the fact
        // stays ungrounded until the control plane checks it.
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
        description = "Find facts by entity, relation, text, or groundedness. Returns each match plus a resource link to its full epistemics.",
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
        let query = p.query.to_lowercase();
        let facts = store.all_facts().map_err(|e| e.to_string())?;
        let hits: Vec<&Fact> = facts
            .iter()
            .filter(|f| {
                query.is_empty()
                    || f.claim.key().to_lowercase().contains(&query)
                    || f.value.render().to_lowercase().contains(&query)
            })
            .filter(|f| p.entity.as_ref().is_none_or(|e| &f.claim.entity.0 == e))
            .filter(|f| p.relation.as_ref().is_none_or(|r| &f.claim.relation == r))
            .filter(|f| {
                p.grounded
                    .as_ref()
                    .is_none_or(|g| f.epistemics.groundedness.label() == g.to_lowercase())
            })
            .collect();

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
        description = "List facts currently holding two disagreeing answers.",
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

impl KenServer {
    fn open(&self) -> Result<JjStore, String> {
        JjStore::open(&self.store_path).map_err(|e| format!("store: {e}"))
    }

    fn open_data(&self) -> Result<JjStore, ErrorData> {
        self.open().map_err(|e| ErrorData::internal_error(e, None))
    }
}

// --- shared read helpers, used by both tools and resources ---

fn recall_output(store: &JjStore, key: &str) -> Result<RecallOutput, String> {
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

fn conflicts_value(store: &JjStore) -> Result<Value, String> {
    let ids = store.list_conflicts().map_err(|e| e.to_string())?;
    let facts = store.all_facts().map_err(|e| e.to_string())?;
    let conflicts: Vec<Value> = facts
        .into_iter()
        .filter(|f| ids.contains(&f.id))
        .map(|f| json!({ "key": f.claim.key(), "value": f.value.render() }))
        .collect();
    Ok(json!({ "conflicts": conflicts }))
}

/// Facts ranked by value of information (DESIGN §7): the ones most likely to be
/// both wrong and consequential. A read-only window onto what the scheduler
/// would check next, so an agent can distrust the memories that are due.
fn stale_value(store: &JjStore) -> Result<Value, String> {
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
fn why_value(store: &JjStore, key: &str) -> Result<Value, String> {
    let fact = store.read_fact_by_key(key).map_err(|e| e.to_string())?;
    let mut ops = store.change_log().map_err(|e| e.to_string())?;
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
        Groundedness::Verified { at, by } => GroundednessOut {
            state: "verified".to_string(),
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
                 checked against a real source, and a conflicted fact holds two answers, so \
                 surface the disagreement rather than picking one. Prefer recently verified, \
                 high-confidence facts; treat stale or ungrounded ones as leads to confirm, \
                 not as settled truth."
            )
        }
        _ => return None,
    };
    Some(vec![PromptMessage::new_text(PromptMessageRole::User, text)])
}

// --- completion: suggest the keys actually in the store ---

fn completion_values(store: &JjStore, arg_name: &str, partial: &str) -> Vec<String> {
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
impl ServerHandler for KenServer {
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
        let service = KenServer { store_path }.serve(stdio()).await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}
