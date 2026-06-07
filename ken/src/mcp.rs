//! `ken mcp`: a data-plane-only MCP server over stdio, built on the official
//! `rmcp` SDK. Agents ingest and recall; they cannot verify, override, or grant,
//! because the interface boundary is the trust boundary. This module never
//! imports `ken::verify::authority`, so no control-plane op is reachable here.

use std::path::PathBuf;

use ken::decay::decayed_confidence;
use ken::ground::render_ground;
use ken::schema::{Claim, Fact, FactValue, Groundedness, TriageSource, Volatility};
use ken::store::{JjStore, VersionedStore};
use ken::write::WriteOp;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{
    schemars, tool, tool_handler, tool_router, transport::stdio, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};

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

#[tool_router]
impl KenServer {
    #[tool(
        description = "Recall a fact: value plus full epistemics (same shape as `ken recall --json`)."
    )]
    fn ken_recall(&self, Parameters(p): Parameters<RecallParams>) -> Result<String, String> {
        let store = self.open()?;
        let fact = store.read_fact_by_key(&p.key).map_err(|e| e.to_string())?;
        let now = chrono::Utc::now();
        let conf = decayed_confidence(&fact, now, store.config());
        let groundedness = match &fact.epistemics.groundedness {
            Groundedness::Ungrounded { source } => {
                json!({ "state": "ungrounded", "source": source })
            }
            Groundedness::Verified { at, by } => {
                json!({ "state": "verified", "at": at, "verifier": by.0 })
            }
            Groundedness::Conflicted { verifiers } => {
                json!({ "state": "conflicted", "verifiers": verifiers })
            }
        };
        let out = json!({
            "key": p.key,
            "value": value_of(&fact.value),
            "confidence": (conf * 100.0).round() / 100.0,
            "groundedness": groundedness,
            "grounds": grounds_of(&fact),
            "last_verified": fact.schedule.last_verified,
        });
        Ok(serde_json::to_string_pretty(&out).unwrap_or_default())
    }

    #[tool(description = "Add a fact. Always lands Ungrounded. Returns the fact id.")]
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
        // Data plane only: the single op untrusted callers can construct.
        let op = WriteOp::ingest(
            claim,
            FactValue::Scalar {
                value: Value::String(p.value),
            },
            triage,
            volatility,
            None,
        );
        let id = store.apply(op).map_err(|e| e.to_string())?;
        Ok(json!({ "id": id.0, "key": p.key, "groundedness": "ungrounded" }).to_string())
    }

    #[tool(
        description = "Find facts by entity, relation, text, or groundedness; returns value plus epistemics."
    )]
    fn ken_search(&self, Parameters(p): Parameters<SearchParams>) -> Result<String, String> {
        let store = self.open()?;
        let now = chrono::Utc::now();
        let query = p.query.to_lowercase();
        let facts = store.all_facts().map_err(|e| e.to_string())?;
        let hits: Vec<Value> = facts
            .into_iter()
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
            .map(|f| {
                let conf = decayed_confidence(&f, now, store.config());
                json!({
                    "key": f.claim.key(),
                    "value": value_of(&f.value),
                    "confidence": (conf * 100.0).round() / 100.0,
                    "groundedness": f.epistemics.groundedness.label(),
                    "grounds": grounds_of(&f),
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&json!({ "matches": hits })).unwrap_or_default())
    }

    #[tool(description = "List facts currently holding two answers.")]
    fn ken_conflicts(&self) -> Result<String, String> {
        let store = self.open()?;
        let ids = store.list_conflicts().map_err(|e| e.to_string())?;
        let facts = store.all_facts().map_err(|e| e.to_string())?;
        let conflicts: Vec<Value> = facts
            .into_iter()
            .filter(|f| ids.contains(&f.id))
            .map(|f| json!({ "key": f.claim.key(), "value": f.value.render() }))
            .collect();
        Ok(serde_json::to_string_pretty(&json!({ "conflicts": conflicts })).unwrap_or_default())
    }
}

impl KenServer {
    fn open(&self) -> Result<JjStore, String> {
        JjStore::open(&self.store_path).map_err(|e| format!("store: {e}"))
    }
}

fn value_of(v: &FactValue) -> Value {
    match v {
        FactValue::Scalar { value } => value.clone(),
        FactValue::Set { elements } => {
            Value::Array(elements.iter().map(|e| e.value.clone()).collect())
        }
    }
}

fn grounds_of(fact: &Fact) -> Value {
    let arr: Vec<Value> = fact
        .grounds
        .iter()
        .map(|g| {
            json!({
                "source": render_ground(g),
                "kind": g.source.kind_label(),
                "predicate": g.predicate.label(),
                "last": g.last.as_ref().map(|r| json!({
                    "rev": r.rev,
                    "span_hash": r.span_hash,
                    "outcome": r.outcome.label(),
                    "at": r.at,
                })),
            })
        })
        .collect();
    Value::Array(arr)
}

#[tool_handler]
impl ServerHandler for KenServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ken", env!("CARGO_PKG_VERSION")))
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
