//! End-to-end tests against a real store: a `.ken/` directory of fact files and
//! an append-only op log, no external VCS required.

use jishuken::engine;
use jishuken::ground::locator::parse_source;
use jishuken::predicate::Predicate;
use jishuken::schema::{
    Claim, CommandSource, FactValue, GroundBinding, GroundSource, Groundedness, HalfLife,
    SourceRoot, TriageSource,
};
use jishuken::store::{JishukenStore, OpId};
use jishuken::write::{WriteOp, INGEST_CONFIDENCE_CEILING};

/// Bind a File ground via a locator string + predicate spec, then check it.
fn ground_file(store: &JishukenStore, key: &str, source: &str, predicate: &str) -> Groundedness {
    let (src, locator) = parse_source(source, None).unwrap();
    let binding = GroundBinding {
        source: GroundSource::File(src),
        locator,
        predicate: Predicate::parse(predicate).unwrap(),
        last: None,
    };
    engine::ground(store, key, binding).unwrap()
}

/// Bind a ground via a locator string, resolving handler-backed schemes through
/// config, then check it.
fn ground_via_config(
    store: &JishukenStore,
    key: &str,
    source: &str,
    predicate: &str,
) -> Groundedness {
    let (src, locator) = parse_source(source, None).unwrap();
    let source = jishuken::ground::ground_source_for(store.config(), store.root(), src).unwrap();
    let binding = GroundBinding {
        source,
        locator,
        predicate: Predicate::parse(predicate).unwrap(),
        last: None,
    };
    engine::ground(store, key, binding).unwrap()
}

fn fresh_store() -> (tempfile::TempDir, JishukenStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = JishukenStore::init(&dir.path().join(".ken")).unwrap();
    (dir, store)
}

fn ingest(
    store: &JishukenStore,
    key: &str,
    value: &str,
    vol: HalfLife,
) -> jishuken::schema::FactId {
    let claim = Claim::parse_key(key).unwrap();
    store
        .apply(WriteOp::ingest(
            claim,
            FactValue::Scalar {
                value: serde_json::Value::String(value.into()),
            },
            TriageSource::Llm {
                meta_confidence: 0.95,
            },
            vol,
            None,
        ))
        .unwrap()
}

#[test]
fn init_add_recall_roundtrip() {
    let (_dir, store) = fresh_store();
    ingest(
        &store,
        "staging.url",
        "https://stg.example.com",
        "PT6H".parse::<HalfLife>().unwrap(),
    );

    let fact = store.read_fact_by_key("staging.url").unwrap();
    assert_eq!(fact.value.render(), "https://stg.example.com");
    // Data-plane ingest always lands Ungrounded.
    assert!(matches!(
        fact.epistemics.groundedness,
        Groundedness::Ungrounded { .. }
    ));
    // ...and cannot exceed the triage ceiling even with a hot meta-confidence.
    assert!(fact.epistemics.confidence <= INGEST_CONFIDENCE_CEILING);
}

#[test]
fn ingest_is_one_tagged_op_in_the_log() {
    let (_dir, store) = fresh_store();
    ingest(
        &store,
        "db.host",
        "10.0.0.1",
        "P90D".parse::<HalfLife>().unwrap(),
    );
    let ops = store.op_log(None).unwrap();
    assert!(
        ops.iter()
            .any(|o| o.description.contains("[Ingest]") && o.description.contains("db.host")),
        "op log should contain a tagged Ingest op: {ops:?}"
    );
}

#[test]
fn recall_reads_the_ingested_fact_file() {
    let (_dir, store) = fresh_store();
    ingest(&store, "svc.port", "8080", HalfLife::default());
    let fact = store.read_fact_by_key("svc.port").unwrap();
    assert_eq!(fact.value.render(), "8080");
}

#[test]
fn undo_rolls_back_one_operation() {
    let (_dir, store) = fresh_store();
    for value in ["A", "B", "C"] {
        ingest(&store, "a.b", value, HalfLife::default());
    }
    assert_eq!(store.op_log(None).unwrap().len(), 4);
    store.undo().unwrap();
    assert_eq!(store.read_fact_by_key("a.b").unwrap().value.render(), "B");
    store.undo().unwrap();
    assert_eq!(store.read_fact_by_key("a.b").unwrap().value.render(), "A");
    assert_eq!(store.op_log(None).unwrap().len(), 2);
    ingest(&store, "a.b", "D", HalfLife::default());
    store.undo().unwrap();
    assert_eq!(store.read_fact_by_key("a.b").unwrap().value.render(), "A");
    store.undo().unwrap();
    assert!(
        store.read_fact_by_key("a.b").is_err(),
        "undo should remove the created fact"
    );
    assert_eq!(store.op_log(None).unwrap().len(), 1);
    assert!(store.undo().is_err());
}

#[test]
fn undo_restores_verification_calibration_and_centrality_together() {
    let (_dir, store) = fresh_store();
    ingest(&store, "release.owner", "alice", HalfLife::default());
    std::fs::write(store.root().join("owner.txt"), "alice").unwrap();
    ground_file(&store, "release.owner", "store:owner.txt", "equals");
    let before = store.read_fact_by_key("release.owner").unwrap();
    let calibration = std::fs::read(store.root().join("calibration.jsonl")).unwrap();
    std::fs::write(store.root().join("owner.txt"), "bob").unwrap();
    assert!(matches!(
        engine::verify_fact(&store, "release.owner").unwrap(),
        Groundedness::Refuted { .. }
    ));
    store.undo().unwrap();
    assert_eq!(store.read_fact_by_key("release.owner").unwrap(), before);
    assert_eq!(
        std::fs::read(store.root().join("calibration.jsonl")).unwrap(),
        calibration
    );
}

#[test]
fn cli_source_hints_require_an_explicit_ground_and_predicate() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("owner.txt"), "The owner is bob.").unwrap();
    ken_cli(
        &store,
        &[
            "add",
            "release.owner",
            "alice",
            "--ground",
            "store:owner.txt",
        ],
    );
    let before = store.read_fact_by_key("release.owner").unwrap();
    assert!(before.grounds.is_empty());
    assert!(!before.is_checkable());
    assert!(before.source_hint.is_some());
    assert!(ken_cli(&store, &["tick"]).contains("nothing due"));
    assert!(engine::verify_fact(&store, "release.owner").is_err());
    assert!(engine::audit_fact(&store, "release.owner", true).is_err());
    assert_eq!(store.read_fact_by_key("release.owner").unwrap(), before);
    let recalled: serde_json::Value =
        serde_json::from_str(&ken_cli(&store, &["recall", "release.owner", "--json"])).unwrap();
    assert_eq!(recalled["source_hint"], "store:owner.txt");
    assert_eq!(recalled["groundedness"]["state"], "ungrounded");
    let missing = std::process::Command::new(env!("CARGO_BIN_EXE_ken"))
        .arg("--store")
        .arg(store.root())
        .args(["ground", "release.owner", "--source", "store:owner.txt"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--predicate"));
    ken_cli(
        &store,
        &[
            "ground",
            "release.owner",
            "--source",
            "store:owner.txt",
            "--predicate",
            "contains",
        ],
    );
    let fact = store.read_fact_by_key("release.owner").unwrap();
    assert!(matches!(
        fact.epistemics.groundedness,
        Groundedness::Refuted { .. }
    ));
    assert!(fact.source_hint.is_none());
}

#[test]
fn cli_normalizes_keys_and_search_filters_without_changing_values() {
    let (_dir, store) = fresh_store();
    let decomposed = "cafe\u{0301}.ro\u{0302}le";
    let value = "value with e\u{0301}";
    ken_cli(&store, &["add", decomposed, value]);
    let recalled: serde_json::Value =
        serde_json::from_str(&ken_cli(&store, &["recall", "café.rôle", "--json"])).unwrap();
    assert_eq!(recalled["key"], "café.rôle");
    assert_eq!(recalled["value"], value);
    let searched: serde_json::Value = serde_json::from_str(&ken_cli(
        &store,
        &[
            "search",
            "--entity",
            "cafe\u{0301}",
            "--relation",
            "ro\u{0302}le",
            "--json",
        ],
    ))
    .unwrap();
    assert_eq!(searched["matches"].as_array().unwrap().len(), 1);
    assert_eq!(searched["matches"][0]["key"], "café.rôle");
}

#[test]
fn discovery_walks_up_for_dot_ken() {
    let (dir, _store) = fresh_store();
    let nested = dir.path().join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    let found = JishukenStore::discover(None, &nested).unwrap();
    assert_eq!(found.root(), dir.path().join(".ken"));
}

#[test]
fn op_log_since_truncates() {
    let (_dir, store) = fresh_store();
    ingest(&store, "x.one", "1", HalfLife::default());
    let mid = store.op_log(None).unwrap();
    let marker = mid.first().unwrap().id.clone();
    ingest(&store, "x.two", "2", HalfLife::default());
    let since = store.op_log(Some(OpId(marker))).unwrap();
    // Everything up to (not including) the marker: only the newer ops remain.
    assert!(since.iter().all(|o| !o.description.contains("x.one")));
}

/// Tier-1 existence: a locator that resolves confirms with no sandbox, and a
/// re-check resolves to the same span hash (the cheap tier-2 path).
#[test]
fn tier1_existence_grounds_and_reconfirms() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("data.txt"), "hello world").unwrap();
    ingest(&store, "doc.greeting", "hello", HalfLife::default());

    let g = ground_file(
        &store,
        "doc.greeting",
        "store:data.txt?q=\"hello\"",
        "exists",
    );
    assert!(matches!(g, Groundedness::Verified { .. }));

    let fact = store.read_fact_by_key("doc.greeting").unwrap();
    let hash1 = fact.grounds[0].last.as_ref().unwrap().span_hash.clone();
    // Re-check (tier 2): the span is unchanged, still confirmed, same hash.
    engine::verify_fact(&store, "doc.greeting").unwrap();
    let fact = store.read_fact_by_key("doc.greeting").unwrap();
    assert_eq!(fact.grounds[0].last.as_ref().unwrap().span_hash, hash1);
}

/// The centerpiece: two independent grounds that disagree drive `Conflicted`
/// and surface in `ken conflicts`. Pure tier-1, no sandbox needed.
#[test]
fn two_independent_grounds_disagree_conflict() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("src.txt"), "auth lives here").unwrap();
    ingest(
        &store,
        "auth.handler",
        "auth lives here",
        HalfLife::default(),
    );

    // Ground A resolves (the quote is present) -> Confirmed.
    ground_file(
        &store,
        "auth.handler",
        "store:src.txt?q=\"auth lives here\"",
        "exists",
    );
    // Ground B does not resolve (the quote is absent) -> Refuted.
    let g = ground_file(
        &store,
        "auth.handler",
        "store:src.txt?q=\"moved elsewhere\"",
        "exists",
    );

    assert!(
        matches!(g, Groundedness::Conflicted { .. }),
        "expected Conflicted, got {g:?}"
    );
    let conflicts = store.list_conflicts().unwrap();
    assert!(conflicts.iter().any(|id| id.0 == "auth.handler"));

    // Once neither quote resolves, both grounds refute the claim.
    std::fs::write(store.root().join("src.txt"), "neither quote is present").unwrap();
    let g = engine::verify_fact(&store, "auth.handler").unwrap();
    assert!(matches!(g, Groundedness::Refuted { .. }));
    assert!(store.list_conflicts().unwrap().is_empty());
}

/// A scheduler tick checks grounded, due facts, and coalesces the centrality
/// recompute into a single `[Centrality]` op for the whole tick (Idea 1),
/// rather than one per check.
#[test]
fn tick_checks_grounded_facts() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("d.txt"), "present").unwrap();
    for key in ["thing.exists", "other.exists", "third.exists"] {
        ingest(&store, key, "present", "PT6H".parse::<HalfLife>().unwrap());
        ground_file(&store, key, "store:d.txt?q=\"present\"", "exists");
    }

    let results = engine::tick(&store).unwrap();
    assert!(results.iter().any(|(k, _)| k == "thing.exists"));

    // Multiple facts were checked, but the tick recomputed centrality once.
    let centrality_ops = store
        .op_log(None)
        .unwrap()
        .into_iter()
        .filter(|o| o.description.contains("[Centrality]"))
        .count();
    assert!(
        centrality_ops <= 1,
        "tick should coalesce to at most one [Centrality] op, got {centrality_ops}"
    );
}

/// A tick with `budget.concurrency > 1` fans the read phase across threads but
/// still applies serially and coalesces centrality into one op (Part B).
#[test]
fn concurrent_tick_checks_all_and_coalesces_centrality() {
    let (_dir, store) = fresh_store();
    // Raise concurrency in the store config, then reopen so it loads.
    let toml = store
        .config()
        .to_toml()
        .replace("concurrency = 1", "concurrency = 4");
    std::fs::write(store.root().join("ken.toml"), toml).unwrap();
    let store = JishukenStore::open(store.root()).unwrap();

    std::fs::write(store.root().join("d.txt"), "present").unwrap();
    let keys = ["a.exists", "b.exists", "c.exists", "d.exists"];
    for key in keys {
        ingest(&store, key, "present", "PT6H".parse::<HalfLife>().unwrap());
        ground_file(&store, key, "store:d.txt?q=\"present\"", "exists");
    }

    let results = engine::tick(&store).unwrap();
    for key in keys {
        assert!(
            results.iter().any(|(k, _)| k == key),
            "{key} should be checked"
        );
    }
    let centrality_ops = store
        .op_log(None)
        .unwrap()
        .into_iter()
        .filter(|o| o.description.contains("[Centrality]"))
        .count();
    assert!(
        centrality_ops <= 1,
        "concurrent tick should still coalesce to one [Centrality] op, got {centrality_ops}"
    );
}

/// The exploration floor: an audit re-checks a confident fact through its
/// independent grounds.
/// When one ground drifts to disagree, the audit surfaces it as Conflicted,
/// never letting the incumbent re-confirm itself.
#[test]
fn audit_through_independent_ground_flips_to_conflicted() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("a.txt"), "auth lives here").unwrap();
    std::fs::write(store.root().join("b.txt"), "auth lives here").unwrap();
    ingest(
        &store,
        "auth.handler",
        "auth lives here",
        HalfLife::default(),
    );

    // Two independent grounds, both confirming -> Verified.
    ground_file(
        &store,
        "auth.handler",
        "store:a.txt?q=\"auth lives here\"",
        "exists",
    );
    let g = ground_file(
        &store,
        "auth.handler",
        "store:b.txt?q=\"auth lives here\"",
        "exists",
    );
    assert!(
        matches!(g, Groundedness::Verified { .. }),
        "expected Verified, got {g:?}"
    );

    // One independent ground drifts: b.txt no longer contains the quote.
    std::fs::write(store.root().join("b.txt"), "moved elsewhere").unwrap();

    let g = engine::audit_fact(&store, "auth.handler", true).unwrap();
    assert!(
        matches!(g, Groundedness::Conflicted { .. }),
        "audit should surface the drift as Conflicted, got {g:?}"
    );
}

/// A `contains` predicate over a File source judges the span against the claim.
#[test]
fn predicate_contains_judges_span() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("cfg.txt"), "port = 8080\nhost = local").unwrap();
    ingest(&store, "svc.port", "8080", HalfLife::default());
    // The span (whole file) contains the claim "8080" -> Confirmed.
    let g = ground_file(&store, "svc.port", "store:cfg.txt", "contains");
    assert!(matches!(g, Groundedness::Verified { .. }));

    // A claim not present -> Refuted.
    ingest(&store, "svc.other", "9999", HalfLife::default());
    let g = ground_file(&store, "svc.other", "store:cfg.txt", "contains");
    let f = store.read_fact_by_key("svc.other").unwrap();
    assert!(matches!(g, Groundedness::Refuted { .. }));
    assert_eq!(f.epistemics.groundedness, g);
    assert!(f.epistemics.confidence < 0.5);
}

/// A `Command` source whose argv[0] is not in the allowlist is refused (errored,
/// no spawn) and never grounds the fact.
#[test]
fn command_allowlist_denies_unlisted_program() {
    let (_dir, store) = fresh_store();
    ingest(
        &store,
        "svc.health",
        "ok",
        "PT6H".parse::<HalfLife>().unwrap(),
    );
    // `[command] allow` is empty by default, so any command is denied.
    let binding = GroundBinding {
        source: GroundSource::Command(CommandSource {
            argv: vec!["echo".into(), "ok".into()],
            root: SourceRoot::Store,
        }),
        locator: jishuken::schema::Locator::Whole,
        predicate: Predicate::Exists,
        last: None,
    };
    let g = engine::ground(&store, "svc.health", binding).unwrap();
    // Errored never moves groundedness off Ungrounded.
    assert!(
        matches!(g, Groundedness::Ungrounded { .. }),
        "denied command must not ground: {g:?}"
    );
}

/// A handler-backed scheme (`wiki:`) mounts code that resolves a reference to
/// bytes; ken then projects its existing locator (`#authentication`) and judges
/// with a pure predicate. Gated on Deno being available.
#[test]
fn handler_scheme_grounds_with_locator_projection() {
    let (_dir, store) = fresh_store();
    if !jishuken::ground::Sandbox::new("deno", std::time::Duration::from_secs(10)).available() {
        eprintln!("skipping: deno not on PATH");
        return;
    }
    let root = store.root().to_path_buf();
    std::fs::create_dir_all(root.join("handlers")).unwrap();
    // The handler synthesizes a Markdown doc from the reference; ken slices the
    // `## Authentication` section out of it.
    std::fs::write(
        root.join("handlers/wiki.ts"),
        "export default { fetch(ref: string) { return `# ${ref}\\n\\n## Authentication\\nauth lives in src/auth.rs\\n\\n## Other\\nx`; } };",
    )
    .unwrap();
    // Mount the wiki scheme as handler-backed, then re-open so config is loaded.
    let cfg = format!(
        "{}\n[sources.wiki]\nhandler = \"handlers/wiki.ts\"\n",
        store.config().to_toml()
    );
    std::fs::write(root.join("ken.toml"), cfg).unwrap();
    let store = JishukenStore::open(&root).unwrap();

    ingest(&store, "auth.handler", "src/auth.rs", HalfLife::default());
    let g = ground_via_config(
        &store,
        "auth.handler",
        "wiki:Architecture#authentication",
        "contains",
    );
    assert!(
        matches!(g, Groundedness::Verified { .. }),
        "handler ground should verify: {g:?}"
    );

    let fact = store.read_fact_by_key("auth.handler").unwrap();
    assert!(
        matches!(fact.grounds[0].source, GroundSource::Handler(_)),
        "the bound source should be a Handler"
    );
}

/// Run the ken binary against a store and return stdout, asserting success.
fn ken_cli(store: &JishukenStore, args: &[&str]) -> String {
    let mut full = vec!["--store", store.root().to_str().unwrap()];
    full.extend_from_slice(args);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ken"))
        .args(&full)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ken {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The full CLI loop a user runs: `add` -> `ground` -> `verify` -> `recall`.
/// The control plane moves groundedness; the recall carries the epistemics.
#[test]
fn cli_ground_verify_recall_roundtrip() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("cfg.txt"), "port = 8080").unwrap();

    ken_cli(&store, &["add", "svc.port", "8080", "--half-life", "3d"]);
    let out = ken_cli(
        &store,
        &[
            "ground",
            "svc.port",
            "--source",
            "store:cfg.txt?q=\"port = 8080\"",
            "--predicate",
            "contains",
        ],
    );
    assert!(out.contains("verified"), "ground should verify: {out}");

    let out = ken_cli(&store, &["verify", "svc.port"]);
    assert!(out.contains("verified"), "re-verify should hold: {out}");

    let out = ken_cli(&store, &["recall", "svc.port", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["value"], "8080");
    assert_eq!(parsed["groundedness"]["state"], "verified");

    // The source changes without the stored claim changing.
    std::fs::write(store.root().join("cfg.txt"), "port = 9090").unwrap();
    let out = ken_cli(&store, &["verify", "svc.port"]);
    assert!(
        out.contains("refuted"),
        "changed source should refute: {out}"
    );
    let out = ken_cli(&store, &["recall", "svc.port"]);
    assert!(
        out.contains("grounded     refuted"),
        "recall should show refutation: {out}"
    );
    let out = ken_cli(&store, &["recall", "svc.port", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["value"], "8080");
    assert_eq!(parsed["groundedness"]["state"], "refuted");
    assert!(parsed["groundedness"]["at"].is_string());
    assert!(parsed["groundedness"]["verifier"].is_string());
    let out = ken_cli(&store, &["search", "--grounded", "refuted", "--json"]);
    let hits: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(hits["matches"].as_array().unwrap().len(), 1);
    assert_eq!(hits["matches"][0]["key"], "svc.port");
    assert_eq!(hits["matches"][0]["groundedness"]["state"], "refuted");

    // Failed reads retain the previous refutation without refreshing evidence.
    let before = store.read_fact_by_key("svc.port").unwrap();
    std::fs::remove_file(store.root().join("cfg.txt")).unwrap();
    engine::verify_fact(&store, "svc.port").unwrap();
    let after = store.read_fact_by_key("svc.port").unwrap();
    assert_eq!(after.epistemics, before.epistemics);
    assert_eq!(after.schedule.last_verified, before.schedule.last_verified);
    assert_eq!(after.grounds, before.grounds);

    // A later confirmation restores the verified state through a ground check.
    std::fs::write(store.root().join("cfg.txt"), "port = 8080").unwrap();
    let g = engine::verify_fact(&store, "svc.port").unwrap();
    assert!(matches!(g, Groundedness::Verified { .. }));
}

/// `ken tick` over the binary checks due grounded facts and reports outcomes.
#[test]
fn cli_tick_checks_due_facts() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("d.txt"), "present").unwrap();
    ken_cli(
        &store,
        &["add", "thing.exists", "present", "--half-life", "6h"],
    );
    ken_cli(
        &store,
        &[
            "ground",
            "thing.exists",
            "--source",
            "store:d.txt?q=\"present\"",
            "--predicate",
            "exists",
        ],
    );

    let out = ken_cli(&store, &["tick"]);
    assert!(
        out.contains("thing.exists") || out.contains("nothing due"),
        "tick should report the checked fact or an empty queue: {out}"
    );
}

/// `ken calibration` reports the samples that ground checks accumulated.
#[test]
fn cli_calibration_reports_accumulated_samples() {
    let (_dir, store) = fresh_store();
    std::fs::write(store.root().join("a.txt"), "value present").unwrap();
    ken_cli(&store, &["add", "one.fact", "value present"]);
    ken_cli(
        &store,
        &[
            "ground",
            "one.fact",
            "--source",
            "store:a.txt?q=\"value present\"",
            "--predicate",
            "equals",
        ],
    );
    // A refuted check logs a second sample.
    ken_cli(&store, &["add", "two.fact", "absent value"]);
    ken_cli(
        &store,
        &[
            "ground",
            "two.fact",
            "--source",
            "store:a.txt?q=\"no such span\"",
            "--predicate",
            "equals",
        ],
    );

    let out = ken_cli(&store, &["calibration", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    let count = parsed["sampleCount"].as_u64().unwrap();
    assert!(count >= 2, "ground checks should log samples: {out}");
    assert!(parsed["brierScore"].is_number());
}

/// `ken search` over the binary lists facts and honors the groundedness filter.
#[test]
fn search_binary_lists_and_filters() {
    let (_dir, store) = fresh_store();
    ingest(&store, "alpha.one", "v1", HalfLife::default());
    ingest(&store, "beta.two", "v2", HalfLife::default());

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ken"))
        .args([
            "--store",
            store.root().to_str().unwrap(),
            "search",
            "alpha",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("alpha.one"),
        "search should find alpha.one: {text}"
    );
    assert!(
        !text.contains("beta.two"),
        "query should exclude beta.two: {text}"
    );

    // Groundedness filter: both are ungrounded, so `--grounded verified` is empty.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ken"))
        .args([
            "--store",
            store.root().to_str().unwrap(),
            "search",
            "--grounded",
            "verified",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("no matches"));
}

#[test]
fn cli_half_lives_use_durations_and_save_the_ingest_default() {
    let (_dir, store) = fresh_store();
    let mut config = store.config().clone();
    config.decay.default_half_life = "2w".parse().unwrap();
    std::fs::write(store.root().join("ken.toml"), config.to_toml()).unwrap();
    ken_cli(&store, &["add", "default.duration", "present"]);
    config.decay.default_half_life = "P1Y".parse().unwrap();
    std::fs::write(store.root().join("ken.toml"), config.to_toml()).unwrap();
    let recalled: serde_json::Value =
        serde_json::from_str(&ken_cli(&store, &["recall", "default.duration", "--json"])).unwrap();
    assert_eq!(recalled["half_life"], "P14D");
    for (input, canonical) in [
        ("P1M", "P1M"),
        ("PT1M", "PT1M"),
        ("1h30m", "PT1H30M"),
        ("1 hour, 30 minutes", "PT1H30M"),
        ("never", "never"),
        ("1ns", "PT0.000000001S"),
    ] {
        ken_cli(
            &store,
            &["add", "explicit.duration", "present", "--half-life", input],
        );
        let recalled: serde_json::Value =
            serde_json::from_str(&ken_cli(&store, &["recall", "explicit.duration", "--json"]))
                .unwrap();
        assert_eq!(recalled["half_life"], canonical);
        assert_eq!(recalled["groundedness"]["state"], "ungrounded");
        let fact = store.read_fact_by_key("explicit.duration").unwrap();
        let expected_due = fact
            .schedule
            .half_life
            .deadline(fact.schedule.last_verified);
        assert_eq!(recalled["due"], serde_json::to_value(expected_due).unwrap());
    }
    let before = std::fs::read(store.root().join("ops.jsonl")).unwrap();
    for input in ["PT0S", "NaN", "typo", "-P1D", "P1DT", "PT1.5H1M"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_ken"))
            .args([
                "--store",
                store.root().to_str().unwrap(),
                "add",
                "explicit.duration",
                "must not replace",
                "--half-life",
                input,
            ])
            .output()
            .unwrap();
        assert!(!out.status.success(), "accepted {input}");
    }
    assert_eq!(
        std::fs::read(store.root().join("ops.jsonl")).unwrap(),
        before
    );
    assert_eq!(
        store
            .read_fact_by_key("explicit.duration")
            .unwrap()
            .value
            .render(),
        "present"
    );
    ken_cli(&store, &["serve", "--once", "--interval", "PT0.5S"]);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ken"))
        .args([
            "--store",
            store.root().to_str().unwrap(),
            "serve",
            "--once",
            "--interval",
            "typo",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
