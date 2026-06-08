//! The `ken` CLI (README "CLI"). Two planes: a data plane (`add`, `recall`,
//! `init`, `search`) any caller may touch, and a control plane (`ground`,
//! `verify`, `doubt`, `grant`) where truth is asserted and privileges granted,
//! logged loudly.

mod mcp;

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};

use ken::calibration::{brier_score, recalibrate, reliability_bins};
use ken::decay::{decayed_confidence, half_life_secs};
use ken::engine;
use ken::ground::locator::parse_source;
use ken::predicate::Predicate;
use ken::scheduler;
use ken::schema::{
    Claim, CommandSource, Element, Epistemics, Fact, FactValue, GroundBinding, GroundSource,
    Groundedness, Locator, SourceRef, SourceRoot, TriageSource, Volatility,
};
use ken::store::{JjStore, VersionedStore};
use ken::verify::authority;
use ken::write::WriteOp;

#[derive(Parser)]
#[command(name = "ken", version, about = "Self-verifying memory for agents.")]
struct Cli {
    /// Store location. Defaults to the nearest `.ken/` walking up from cwd.
    #[arg(long, global = true, env = "KEN_STORE")]
    store: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a `.ken/` store in the current project.
    Init,

    /// Add a fact. Lands `Ungrounded`. Key is `entity.relation`.
    Add(AddArgs),

    /// Recall a fact: value plus full epistemics.
    Recall {
        key: String,
        #[arg(long)]
        json: bool,
    },

    /// Provenance back to the ground sources, with op IDs.
    Why { key: String },

    /// What is due, ranked by value of information.
    Stale {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },

    /// Find facts by entity, relation, or text.
    Search(SearchArgs),

    /// Facts currently holding two answers.
    Conflicts,

    /// The operation log (this is `jj op log` underneath).
    Log,

    /// Roll the store back one operation.
    Undo,

    /// Bind an independent ground source to a fact (control plane). Exactly one
    /// of --source / --command / --generator picks the source kind.
    Ground(GroundArgs),

    /// Force a ground check now (control plane).
    Verify { key: String },

    /// Manual override of belief, logged (control plane).
    Doubt {
        key: String,
        #[arg(long)]
        reason: String,
        /// Confidence to set; defaults to a strong doubt.
        #[arg(long)]
        confidence: Option<f64>,
    },

    /// Entitle a generator with network/read capability (control plane).
    Grant {
        generator: PathBuf,
        #[arg(long = "net", num_args = 1..)]
        net: Vec<String>,
        #[arg(long = "read", num_args = 1..)]
        read: Vec<String>,
    },

    /// Run one scheduler tick: check the top facts by value of information.
    Tick,

    /// Run the scheduler as a resident daemon (one tick per interval).
    Serve {
        /// Interval between ticks, e.g. `60s`, `5m`. Overrides `[daemon]`.
        #[arg(long)]
        interval: Option<String>,
        /// Run a single tick and exit.
        #[arg(long)]
        once: bool,
        /// Install a macOS launch agent targeting this store and exit.
        #[arg(long)]
        install_launch_agent: bool,
        /// Remove the macOS launch agent for this store and exit.
        #[arg(long)]
        uninstall_launch_agent: bool,
    },

    /// Run the data-plane MCP server over stdio.
    Mcp,

    /// Print calibration metrics from the verification log.
    Calibration {
        #[arg(long, default_value_t = 10)]
        bins: usize,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
struct AddArgs {
    key: String,
    /// Scalar value. Omit when using `--json`.
    value: Option<String>,
    /// Read a JSON value (array → set-valued fact) from a file.
    #[arg(long)]
    json: Option<PathBuf>,
    #[arg(long, default_value = "days")]
    volatility: Volatility,
    /// Name where the truth should be checked (a hint; stays Ungrounded
    /// until the control plane grounds it).
    #[arg(long)]
    ground: Option<String>,
    /// LLM triage meta-confidence; clamped to the ingest ceiling.
    #[arg(long)]
    meta_confidence: Option<f64>,
}

#[derive(Args)]
struct SearchArgs {
    #[arg(default_value = "")]
    query: String,
    #[arg(long)]
    entity: Option<String>,
    #[arg(long)]
    relation: Option<String>,
    /// Filter by groundedness: ungrounded | verified | conflicted.
    #[arg(long)]
    grounded: Option<String>,
    /// Only facts already due for re-check.
    #[arg(long)]
    due: bool,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct GroundArgs {
    key: String,
    /// File source locator, e.g. `wiki:Architecture.md#authentication`.
    #[arg(long)]
    source: Option<String>,
    /// Command source, e.g. `curl -s https://api.internal/release`
    /// (argv[0] must be in the `[command] allow` list).
    #[arg(long)]
    command: Option<String>,
    /// Generator source: a sandboxed Deno script that emits the value.
    #[arg(long)]
    generator: Option<PathBuf>,
    /// Pin the source revision (File sources).
    #[arg(long)]
    rev: Option<String>,
    /// How to judge the span: exists | equals[:lit] | contains[:lit] |
    /// matches:<re> | num:<op>:<n> | ptr:<rfc6901>[:<sub>].
    #[arg(long, default_value = "exists")]
    predicate: String,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Command::Init = cli.command {
        let target = match &cli.store {
            Some(p) => p.clone(),
            None => std::env::current_dir()?.join(".ken"),
        };
        let store = JjStore::init(&target)?;
        println!("initialized ken store at {}", store.root().display());
        return Ok(());
    }

    let store = open_store(&cli)?;

    match cli.command {
        Command::Init => unreachable!(),
        Command::Add(args) => cmd_add(&store, args)?,
        Command::Recall { key, json } => cmd_recall(&store, &key, json)?,
        Command::Why { key } => cmd_why(&store, &key)?,
        Command::Stale { limit } => cmd_stale(&store, limit)?,
        Command::Search(args) => cmd_search(&store, args)?,
        Command::Conflicts => cmd_conflicts(&store)?,
        Command::Log => cmd_log(&store)?,
        Command::Undo => {
            store.undo()?;
            println!("rolled back one operation");
        }
        Command::Ground(args) => cmd_ground(&store, args)?,
        Command::Verify { key } => {
            let g = engine::verify_fact(&store, &key)?;
            println!("{key} -> {}", g.label());
        }
        Command::Doubt {
            key,
            reason,
            confidence,
        } => cmd_doubt(&store, &key, reason, confidence)?,
        Command::Grant {
            generator,
            net,
            read,
        } => {
            let r = engine::grant(&store, &generator, &net, &read)?;
            println!(
                "granted {} net=[{}] read=[{}]",
                r.display(),
                net.join(", "),
                read.join(", ")
            );
        }
        Command::Tick => cmd_tick(&store)?,
        Command::Serve {
            interval,
            once,
            install_launch_agent,
            uninstall_launch_agent,
        } => cmd_serve(
            &store,
            interval,
            once,
            install_launch_agent,
            uninstall_launch_agent,
        )?,
        Command::Mcp => mcp::serve(store.root().to_path_buf())?,
        Command::Calibration { bins, json } => cmd_calibration(&store, bins, json)?,
    }
    Ok(())
}

fn cmd_calibration(store: &JjStore, bins: usize, json: bool) -> anyhow::Result<()> {
    let samples = store.calibration_samples()?;
    let brier = brier_score(&samples);
    let reliability = reliability_bins(&samples, bins);
    let recalibrator = recalibrate(&samples);

    if json {
        let out = serde_json::json!({
            "sampleCount": samples.len(),
            "brierScore": brier,
            "reliabilityBins": reliability,
            "recalibrator": { "a": recalibrator.a, "b": recalibrator.b },
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("calibration samples: {}", samples.len());
    println!("brier score:         {brier:.4}");
    println!("recalibrator:        a={:.4} b={:.4}", recalibrator.a, recalibrator.b);
    if reliability.is_empty() {
        println!("reliability bins:    (none)");
    } else {
        println!("reliability bins:");
        for b in reliability {
            println!(
                "  center={:.2}  predicted={:.3}  observed={:.3}  n={}",
                b.bin_center, b.mean_predicted, b.mean_observed, b.count
            );
        }
    }
    Ok(())
}

fn open_store(cli: &Cli) -> anyhow::Result<JjStore> {
    let cwd = std::env::current_dir()?;
    Ok(JjStore::discover(cli.store.as_deref(), &cwd)?)
}

fn cmd_add(
    store: &JjStore,
    AddArgs {
        key,
        value,
        json,
        volatility,
        ground,
        meta_confidence,
    }: AddArgs,
) -> anyhow::Result<()> {
    let claim = Claim::parse_key(&key)?;
    let value = match (json, value) {
        (Some(path), _) => {
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
            match v {
                serde_json::Value::Array(items) => FactValue::Set {
                    elements: items
                        .into_iter()
                        .map(|value| Element {
                            value,
                            seen: Utc::now(),
                            confidence: None,
                            conflict: None,
                        })
                        .collect(),
                },
                scalar => FactValue::Scalar { value: scalar },
            }
        }
        (None, Some(v)) => FactValue::Scalar {
            value: serde_json::Value::String(v),
        },
        (None, None) => anyhow::bail!("provide a value or --json <file>"),
    };

    let triage = match meta_confidence {
        Some(mc) => TriageSource::Llm {
            meta_confidence: mc,
        },
        None => TriageSource::Ingest,
    };

    // `--ground` is a hint: a draft File binding with the default `Exists`
    // predicate (Ungrounded until the control plane grounds it).
    let draft_ground = match ground {
        Some(s) => {
            let (src, locator) = parse_source(&s, None)?;
            Some(GroundBinding {
                source: source_to_ground(store, src)?,
                locator,
                predicate: Predicate::Exists,
                last: None,
            })
        }
        None => None,
    };

    let id = store.apply(WriteOp::ingest(
        claim,
        value,
        triage,
        volatility,
        draft_ground,
    ))?;
    println!("{key} ingested ungrounded ({id})");
    Ok(())
}

/// Turn a parsed source reference into a ground source (handler-backed scheme
/// or plain file), capturing any handler's content hash now.
fn source_to_ground(store: &JjStore, src: SourceRef) -> anyhow::Result<GroundSource> {
    Ok(ken::ground::ground_source_for(
        store.config(),
        store.root(),
        src,
    )?)
}

fn cmd_ground(
    store: &JjStore,
    GroundArgs {
        key,
        source,
        command,
        generator,
        rev,
        predicate,
    }: GroundArgs,
) -> anyhow::Result<()> {
    let predicate = Predicate::parse(&predicate)?;
    let (ground_source, locator) = match (source, command, generator) {
        (Some(s), None, None) => {
            let (src, locator) = parse_source(&s, rev)?;
            (source_to_ground(store, src)?, locator)
        }
        (None, Some(cmd), None) => {
            let argv: Vec<String> = cmd.split_whitespace().map(String::from).collect();
            if argv.is_empty() {
                anyhow::bail!("--command is empty");
            }
            (
                GroundSource::Command(CommandSource {
                    argv,
                    root: SourceRoot::Project,
                }),
                Locator::Whole,
            )
        }
        (None, None, Some(path)) => {
            let caps = ken::schema::Capabilities::default(); // network-free until granted
            let src = ken::ground::generator::GeneratorRegistry::load_src(&path, caps)?;
            let r = store.registry().put(&src)?;
            (GroundSource::Generator(r), Locator::Whole)
        }
        _ => anyhow::bail!("provide exactly one of --source, --command, or --generator"),
    };
    let binding = GroundBinding {
        source: ground_source,
        locator,
        predicate,
        last: None,
    };
    let g = engine::ground(store, &key, binding)?;
    println!("{key} grounded -> {}", g.label());
    Ok(())
}

fn cmd_recall(store: &JjStore, key: &str, json: bool) -> anyhow::Result<()> {
    let fact = store.read_fact_by_key(key)?;
    let now = Utc::now();
    let conf = decayed_confidence(&fact, now, store.config());
    let due = due_at(&fact, store);

    if json {
        let out = serde_json::json!({
            "key": key,
            "value": value_json(&fact.value),
            "confidence": round2(conf),
            "groundedness": groundedness_json(&fact.epistemics.groundedness),
            "grounds": grounds_json(&fact),
            "last_verified": fact.schedule.last_verified,
            "due": due,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("{key} = {}", fact.value.render());
    println!(
        "  confidence   {:.2}   volatility={}",
        conf,
        volatility_label(fact.schedule.volatility)
    );
    match &fact.epistemics.groundedness {
        Groundedness::Ungrounded { .. } => {
            let n = fact.grounds.len();
            if n == 0 {
                println!("  grounded     ungrounded");
            } else {
                println!("  grounded     ungrounded ({n} ground(s) bound, unchecked)");
            }
        }
        Groundedness::Verified { at, .. } => {
            println!(
                "  grounded     verified {} · {}",
                relative(*at, now),
                confirming_ground(&fact).unwrap_or_else(|| "existence".to_string())
            );
        }
        Groundedness::Conflicted { verifiers } => {
            println!("  grounded     conflicted ({} grounds)", verifiers.len());
        }
    }
    match due {
        Some(d) => println!("  due          {}", relative_future(d, now)),
        None => println!("  due          never"),
    }
    Ok(())
}

fn cmd_why(store: &JjStore, key: &str) -> anyhow::Result<()> {
    let fact = store.read_fact_by_key(key)?;
    println!("{key} = {}", fact.value.render());
    println!(
        "  ingested   {} by {}  (confidence {:.2}, ungrounded)",
        fact.provenance.ingested_at.format("%Y-%m-%d"),
        fact.provenance.ingested_by,
        ken::write::landed_confidence(&TriageSource::Ingest).min(fact.epistemics.confidence),
    );

    // Ground-check operations for this fact, oldest first (tagged change log).
    let mut ops = store.change_log()?;
    ops.reverse();
    for op in &ops {
        if op.description.contains("[GroundCheck") && op.description.contains(key) {
            let outcome = op
                .description
                .split(':')
                .nth(1)
                .and_then(|s| s.split(']').next())
                .unwrap_or("?");
            println!(
                "  checked    {}  -> {}  (op {})",
                op.time.split(' ').next().unwrap_or(&op.time),
                outcome,
                short_op(&op.id),
            );
        }
    }

    // Each ground: source, kind, predicate, span-hash prefix, last outcome.
    for g in &fact.grounds {
        let span = g.last.as_ref().map_or_else(
            || "  unchecked".to_string(),
            |r| {
                if r.span_hash.is_empty() {
                    format!("  {}", r.outcome.label())
                } else {
                    format!(
                        "  span {}  {}",
                        &r.span_hash[..r.span_hash.len().min(6)],
                        r.outcome.label()
                    )
                }
            },
        );
        println!(
            "  ground     {}  {}  [{}]{}",
            g.source.kind_label(),
            ken::ground::render_ground(g),
            g.predicate.label(),
            span
        );
    }

    // The conflict line: the disagreement made into data.
    if let Groundedness::Conflicted { .. } = &fact.epistemics.groundedness {
        println!("  conflict   independent grounds disagree on the value");
    }
    Ok(())
}

fn cmd_stale(store: &JjStore, limit: usize) -> anyhow::Result<()> {
    let facts = store.all_facts()?;
    let now = Utc::now();
    let costs = store.cost_table();
    let ranked = scheduler::rank(&facts, now, store.config(), &costs);
    if ranked.is_empty() {
        println!("no facts");
        return Ok(());
    }
    for f in ranked.into_iter().take(limit) {
        let voi = scheduler::voi_score(f, now, store.config(), &costs);
        let conf = decayed_confidence(f, now, store.config());
        println!(
            "{:<28} voi={:>7.3}  conf={:.2}  {}",
            f.claim.key(),
            voi,
            conf,
            f.epistemics.groundedness.label(),
        );
    }
    Ok(())
}

fn cmd_search(
    store: &JjStore,
    SearchArgs {
        query,
        entity,
        relation,
        grounded,
        due,
        limit,
        json,
    }: SearchArgs,
) -> anyhow::Result<()> {
    let facts = store.all_facts()?;
    let now = Utc::now();
    let costs = store.cost_table();
    let q = query.to_lowercase();

    let mut hits: Vec<&Fact> = facts
        .iter()
        .filter(|f| {
            q.is_empty()
                || f.claim.key().to_lowercase().contains(&q)
                || f.value.render().to_lowercase().contains(&q)
        })
        .filter(|f| entity.as_ref().is_none_or(|e| &f.claim.entity.0 == e))
        .filter(|f| relation.as_ref().is_none_or(|r| &f.claim.relation == r))
        .filter(|f| {
            grounded
                .as_ref()
                .is_none_or(|g| f.epistemics.groundedness.label() == g.to_lowercase())
        })
        .filter(|f| !due || is_due(f, store, now))
        .collect();

    hits.sort_by(|a, b| {
        scheduler::voi_score(b, now, store.config(), &costs).total_cmp(&scheduler::voi_score(
            a,
            now,
            store.config(),
            &costs,
        ))
    });
    hits.truncate(limit);

    if json {
        let arr: Vec<_> = hits
            .iter()
            .map(|f| {
                serde_json::json!({
                    "key": f.claim.key(),
                    "value": value_json(&f.value),
                    "confidence": round2(decayed_confidence(f, now, store.config())),
                    "groundedness": groundedness_json(&f.epistemics.groundedness),
                    "grounds": grounds_json(f),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "matches": arr }))?
        );
        return Ok(());
    }

    if hits.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for f in hits {
        println!(
            "{:<28} {:<10} conf={:.2}  {}",
            f.claim.key(),
            f.epistemics.groundedness.label(),
            decayed_confidence(f, now, store.config()),
            truncate(&f.value.render(), 40),
        );
    }
    Ok(())
}

fn cmd_conflicts(store: &JjStore) -> anyhow::Result<()> {
    let ids = store.list_conflicts()?;
    if ids.is_empty() {
        println!("no conflicts");
        return Ok(());
    }
    for f in store.all_facts()? {
        if ids.contains(&f.id) {
            println!("{}  ({})", f.claim.key(), f.value.render());
        }
    }
    Ok(())
}

fn cmd_log(store: &JjStore) -> anyhow::Result<()> {
    for op in store.op_log(None)? {
        println!("{}  {}  {}", short_op(&op.id), op.time, op.description);
    }
    Ok(())
}

fn cmd_doubt(
    store: &JjStore,
    key: &str,
    reason: String,
    confidence: Option<f64>,
) -> anyhow::Result<()> {
    let fact = store.read_fact_by_key(key)?;
    let conf = confidence.unwrap_or(0.1).clamp(0.0, 1.0);
    let set = Epistemics {
        confidence: conf,
        groundedness: Groundedness::Ungrounded {
            source: TriageSource::Manual,
        },
    };
    store.apply(authority::manual_override(fact.id, set, reason.clone()))?;
    println!("{key} doubted (confidence {conf:.2}): {reason}");
    Ok(())
}

fn cmd_tick(store: &JjStore) -> anyhow::Result<()> {
    let results = engine::tick(store)?;
    if results.is_empty() {
        println!("nothing due");
        return Ok(());
    }
    for (key, g) in results {
        println!("{key} -> {}", g.label());
    }
    Ok(())
}

fn cmd_serve(
    store: &JjStore,
    interval: Option<String>,
    once: bool,
    install: bool,
    uninstall: bool,
) -> anyhow::Result<()> {
    if install {
        let path = launch_agent::install(store.root())?;
        println!("installed launch agent at {}", path.display());
        return Ok(());
    }
    if uninstall {
        let removed = launch_agent::uninstall(store.root())?;
        println!(
            "{}",
            if removed {
                "removed launch agent"
            } else {
                "no launch agent installed"
            }
        );
        return Ok(());
    }

    let secs = interval
        .as_deref()
        .and_then(ken::config::parse_duration_secs)
        .unwrap_or_else(|| store.config().daemon.interval_secs());

    loop {
        match engine::tick(store) {
            Ok(results) => {
                for (key, g) in results {
                    println!("[tick] {key} -> {}", g.label());
                }
            }
            Err(e) => eprintln!("[tick] error: {e}"),
        }
        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(secs.max(1.0)));
    }
    Ok(())
}

// --- macOS launch agent ---

mod launch_agent {
    use std::path::{Path, PathBuf};

    fn label(store_root: &Path) -> String {
        let h = blake3::hash(store_root.to_string_lossy().as_bytes()).to_hex();
        format!("dev.ken.{}", &h[..12])
    }

    fn plist_path(store_root: &Path) -> anyhow::Result<PathBuf> {
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
        Ok(PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", label(store_root))))
    }

    pub fn install(store_root: &Path) -> anyhow::Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let label = label(store_root);
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>serve</string>
    <string>--store</string>
    <string>{store}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict>
</plist>
"#,
            label = label,
            exe = exe.display(),
            store = store_root.display(),
        );
        let path = plist_path(store_root)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, plist)?;
        Ok(path)
    }

    pub fn uninstall(store_root: &Path) -> anyhow::Result<bool> {
        let path = plist_path(store_root)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

// --- formatting helpers ---

fn value_json(v: &FactValue) -> serde_json::Value {
    match v {
        FactValue::Scalar { value } => value.clone(),
        FactValue::Set { elements } => {
            serde_json::Value::Array(elements.iter().map(|e| e.value.clone()).collect())
        }
    }
}

fn groundedness_json(g: &Groundedness) -> serde_json::Value {
    match g {
        Groundedness::Ungrounded { source } => serde_json::json!({
            "state": "ungrounded",
            "source": source,
        }),
        Groundedness::Verified { at, by } => serde_json::json!({
            "state": "verified",
            "at": at,
            "verifier": by.0,
        }),
        Groundedness::Conflicted { verifiers } => serde_json::json!({
            "state": "conflicted",
            "verifiers": verifiers,
        }),
    }
}

fn grounds_json(fact: &Fact) -> serde_json::Value {
    let arr: Vec<_> = fact
        .grounds
        .iter()
        .map(|g| {
            serde_json::json!({
                "source": ken::ground::render_ground(g),
                "kind": g.source.kind_label(),
                "predicate": g.predicate.label(),
                "last": g.last.as_ref().map(|r| serde_json::json!({
                    "rev": r.rev,
                    "span_hash": r.span_hash,
                    "outcome": r.outcome.label(),
                    "at": r.at,
                })),
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

/// The display of whichever ground last confirmed the fact, for the recall line.
fn confirming_ground(fact: &Fact) -> Option<String> {
    fact.grounds
        .iter()
        .filter(|g| {
            g.last
                .as_ref()
                .is_some_and(|r| r.outcome == ken::schema::Outcome::Confirmed)
        })
        .max_by(|a, b| {
            a.last
                .as_ref()
                .unwrap()
                .at
                .cmp(&b.last.as_ref().unwrap().at)
        })
        .map(ken::ground::render_ground)
}

fn volatility_label(v: Volatility) -> &'static str {
    match v {
        Volatility::Immutable => "immutable",
        Volatility::Slow => "slow",
        Volatility::Days => "days",
        Volatility::Hours => "hours",
    }
}

fn due_at(fact: &Fact, store: &JjStore) -> Option<DateTime<Utc>> {
    let hl = half_life_secs(fact.schedule.volatility, store.config())?;
    Some(fact.schedule.last_verified + chrono::Duration::seconds(hl as i64))
}

fn is_due(fact: &Fact, store: &JjStore, now: DateTime<Utc>) -> bool {
    due_at(fact, store).is_some_and(|d| d <= now)
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn short_op(id: &str) -> String {
    id.chars().take(4).collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn relative(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    format!("{} ago", humanize(secs))
}

fn relative_future(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (then - now).num_seconds();
    if secs <= 0 {
        "overdue".to_string()
    } else {
        format!("in {}", humanize(secs))
    }
}

fn humanize(secs: i64) -> String {
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 5400 {
        format!("{}m", secs / 60)
    } else if secs < 172_800 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
