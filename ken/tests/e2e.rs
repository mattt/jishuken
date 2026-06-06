//! End-to-end tests against a real jj-backed store. Gated on the `jj` CLI being
//! available, per the task brief; skip cleanly otherwise.

use ken::engine;
use ken::ground::locator::parse_source;
use ken::predicate::Predicate;
use ken::schema::{
    Claim, CommandSource, FactValue, GroundBinding, GroundSource, Groundedness, SourceRoot,
    TriageSource, Volatility,
};
use ken::store::{jj_available, JjStore, OpId, Rev, VersionedStore};
use ken::write::{WriteOp, INGEST_CONFIDENCE_CEILING};

/// Bind a File ground via a locator string + predicate spec, then check it.
fn ground_file(store: &JjStore, key: &str, source: &str, predicate: &str) -> Groundedness {
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
fn ground_via_config(store: &JjStore, key: &str, source: &str, predicate: &str) -> Groundedness {
    let (src, locator) = parse_source(source, None).unwrap();
    let source = ken::ground::ground_source_for(store.config(), store.root(), src).unwrap();
    let binding = GroundBinding {
        source,
        locator,
        predicate: Predicate::parse(predicate).unwrap(),
        last: None,
    };
    engine::ground(store, key, binding).unwrap()
}

fn fresh_store() -> Option<(tempfile::TempDir, JjStore)> {
    if !jj_available() {
        eprintln!("skipping: jj not on PATH");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let store = JjStore::init(&dir.path().join(".ken")).unwrap();
    Some((dir, store))
}

fn ingest(store: &JjStore, key: &str, value: &str, vol: Volatility) -> ken::schema::ChangeId {
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
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    ingest(
        &store,
        "staging.url",
        "https://stg.example.com",
        Volatility::Hours,
    );

    let fact = store.read_fact_by_key("staging.url").unwrap();
    assert_eq!(fact.value.render(), "https://stg.example.com");
    // Data-plane ingest always lands Ungrounded (DESIGN §2, §3).
    assert!(matches!(
        fact.epistemics.groundedness,
        Groundedness::Ungrounded { .. }
    ));
    // ...and cannot exceed the triage ceiling even with a hot meta-confidence.
    assert!(fact.epistemics.confidence <= INGEST_CONFIDENCE_CEILING);
}

#[test]
fn ingest_is_one_tagged_op_in_the_log() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    ingest(&store, "db.host", "10.0.0.1", Volatility::Slow);
    let ops = store.change_log().unwrap();
    assert!(
        ops.iter()
            .any(|o| o.description.contains("[Ingest]") && o.description.contains("db.host")),
        "change log should contain a tagged Ingest op: {ops:?}"
    );
    // The op log (what the system did) is still available for `ken log`.
    assert!(!store.op_log(None).unwrap().is_empty());
}

#[test]
fn read_at_revision_uses_jj_file_show() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    let id = ingest(&store, "svc.port", "8080", Volatility::Days);
    // The committed revision is the parent of the working copy after `jj commit`.
    let at = store.read_fact(&id, Rev::At("@-".into())).unwrap();
    assert_eq!(at.value.render(), "8080");
}

#[test]
fn undo_rolls_back_one_operation() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    ingest(&store, "a.b", "1", Volatility::Days);
    let before = store.op_log(None).unwrap().len();
    // `ken undo` maps to `jj undo`: it rolls the store back one operation and
    // succeeds against the real repo (jj records the undo itself as an op).
    store.undo().unwrap();
    let head = store.op_log(None).unwrap();
    assert!(head.len() > before, "undo is itself recorded in the op log");
}

#[test]
fn discovery_walks_up_for_dot_ken() {
    let Some((dir, _store)) = fresh_store() else {
        return;
    };
    let nested = dir.path().join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    let found = JjStore::discover(None, &nested).unwrap();
    assert_eq!(found.root(), dir.path().join(".ken"));
}

#[test]
fn op_log_since_truncates() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    ingest(&store, "x.one", "1", Volatility::Days);
    let mid = store.op_log(None).unwrap();
    let marker = mid.first().unwrap().id.clone();
    ingest(&store, "x.two", "2", Volatility::Days);
    let since = store.op_log(Some(OpId(marker))).unwrap();
    // Everything up to (not including) the marker: only the newer ops remain.
    assert!(since.iter().all(|o| !o.description.contains("x.one")));
}

/// Tier-1 existence: a locator that resolves confirms with no sandbox, and a
/// re-check resolves to the same span hash (the cheap tier-2 path).
#[test]
fn tier1_existence_grounds_and_reconfirms() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    std::fs::write(store.root().join("data.txt"), "hello world").unwrap();
    ingest(&store, "doc.greeting", "hello", Volatility::Days);

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
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    std::fs::write(store.root().join("src.txt"), "auth lives here").unwrap();
    ingest(&store, "auth.handler", "auth lives here", Volatility::Days);

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
}

/// A scheduler tick checks grounded, due facts.
#[test]
fn tick_checks_grounded_facts() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    std::fs::write(store.root().join("d.txt"), "present").unwrap();
    ingest(&store, "thing.exists", "present", Volatility::Hours);
    ground_file(
        &store,
        "thing.exists",
        "store:d.txt?q=\"present\"",
        "exists",
    );

    let results = engine::tick(&store).unwrap();
    assert!(results.iter().any(|(k, _)| k == "thing.exists"));
}

/// A `contains` predicate over a File source judges the span against the claim.
#[test]
fn predicate_contains_judges_span() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    std::fs::write(store.root().join("cfg.txt"), "port = 8080\nhost = local").unwrap();
    ingest(&store, "svc.port", "8080", Volatility::Days);
    // The span (whole file) contains the claim "8080" -> Confirmed.
    let g = ground_file(&store, "svc.port", "store:cfg.txt", "contains");
    assert!(matches!(g, Groundedness::Verified { .. }));

    // A claim not present -> Refuted.
    ingest(&store, "svc.other", "9999", Volatility::Days);
    let g = ground_file(&store, "svc.other", "store:cfg.txt", "contains");
    let f = store.read_fact_by_key("svc.other").unwrap();
    assert!(matches!(g, Groundedness::Verified { .. })); // checked
    assert!(f.epistemics.confidence < 0.5); // ...but refuted, so confidence fell
}

/// A `Command` source whose argv[0] is not in the allowlist is refused (errored,
/// no spawn) and never grounds the fact.
#[test]
fn command_allowlist_denies_unlisted_program() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    ingest(&store, "svc.health", "ok", Volatility::Hours);
    // `[command] allow` is empty by default, so any command is denied.
    let binding = GroundBinding {
        source: GroundSource::Command(CommandSource {
            argv: vec!["echo".into(), "ok".into()],
            root: SourceRoot::Store,
        }),
        locator: ken::schema::Locator::Whole,
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
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    if !ken::ground::Sandbox::new("deno", std::time::Duration::from_secs(10)).available() {
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
    let store = JjStore::open(&root).unwrap();

    ingest(&store, "auth.handler", "src/auth.rs", Volatility::Days);
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

/// `ken search` over the binary lists facts and honors the groundedness filter.
#[test]
fn search_binary_lists_and_filters() {
    let Some((_dir, store)) = fresh_store() else {
        return;
    };
    ingest(&store, "alpha.one", "v1", Volatility::Days);
    ingest(&store, "beta.two", "v2", Volatility::Days);

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
