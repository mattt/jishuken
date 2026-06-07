//! Source generators: sandboxed code that *produces* a ground value (DESIGN
//! §6a, repurposed). This is the reading layer's escape hatch for the
//! authenticate-fetch-normalize case (the Makefile `curl | jq` target). The
//! capability set lives inside the content hash, so a generator that starts
//! asking for the network produces a loud diff and needs a fresh grant. The
//! generator's stdout is the value; a pure predicate judges it later. No
//! write-back to the store; hard timeout.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::{Capabilities, GeneratorHash, GeneratorRef, HandlerRef};

/// LLM-drafted generator source plus its declared capabilities and cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratorSrc {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub caps: Capabilities,
    #[serde(default = "default_cost")]
    pub cost_estimate: f64,
}

fn default_cost() -> f64 {
    1.0
}

impl GeneratorSrc {
    pub fn new(name: impl Into<String>, source: impl Into<String>, caps: Capabilities) -> Self {
        let net = !caps.net.is_empty();
        GeneratorSrc {
            name: name.into(),
            source: source.into(),
            caps,
            cost_estimate: if net { 10.0 } else { 3.0 },
        }
    }

    /// Hash over source AND capabilities, canonicalized so ordering is stable.
    pub fn hash(&self) -> GeneratorHash {
        let mut net = self.caps.net.clone();
        let mut read = self.caps.read.clone();
        let mut env = self.caps.env.clone();
        net.sort();
        read.sort();
        env.sort();
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.source.as_bytes());
        hasher.update(b"\x00net\x00");
        hasher.update(net.join(",").as_bytes());
        hasher.update(b"\x00read\x00");
        hasher.update(read.join(",").as_bytes());
        hasher.update(b"\x00env\x00");
        hasher.update(env.join(",").as_bytes());
        GeneratorHash(hasher.finalize().to_hex().to_string())
    }

    pub fn to_ref(&self) -> GeneratorRef {
        let hash = self.hash();
        GeneratorRef {
            src_path: format!("verifiers/{}.ts", &hash.0[..hash.0.len().min(16)]),
            hash,
            caps: self.caps.clone(),
            cost_estimate: self.cost_estimate,
            name: self.name.clone(),
        }
    }
}

/// On-disk store of generator sources under `verifiers/`.
#[derive(Debug)]
pub struct GeneratorRegistry {
    root: PathBuf,
}

impl GeneratorRegistry {
    pub fn new(store_root: &Path) -> Self {
        GeneratorRegistry {
            root: store_root.to_path_buf(),
        }
    }

    /// Persist a generator and return its content-addressed reference.
    ///
    /// # Errors
    /// Returns an error if the source or its metadata cannot be written.
    pub fn put(&self, src: &GeneratorSrc) -> Result<GeneratorRef> {
        let r = src.to_ref();
        let path = self.root.join(&r.src_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &src.source)?;
        let meta = self.root.join(format!("{}.json", r.src_path));
        std::fs::write(&meta, serde_json::to_vec_pretty(src)?)?;
        Ok(r)
    }

    /// Load a generator source from an on-disk `.ts` path. Capabilities default
    /// to network-free.
    ///
    /// # Errors
    /// Returns an error if `path` cannot be read.
    pub fn load_src(path: &Path, caps: Capabilities) -> Result<GeneratorSrc> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| Error::Verifier(format!("reading {}: {e}", path.display())))?;
        let name = path.file_name().map_or_else(
            || "generator.ts".to_string(),
            |s| s.to_string_lossy().to_string(),
        );
        Ok(GeneratorSrc::new(name, source, caps))
    }

    /// Read a registered generator's source text.
    ///
    /// # Errors
    /// Returns an error if the source file cannot be read.
    pub fn read_source(&self, r: &GeneratorRef) -> Result<String> {
        Ok(std::fs::read_to_string(self.root.join(&r.src_path))?)
    }
}

/// Load a scheme handler from its on-disk module under the store root (e.g.
/// `handlers/wiki.ts`) with the capabilities declared for its scheme, and
/// return both the runnable source and its content-addressed reference. The
/// hash covers source AND capabilities, so any drift in either is a loud diff
/// (DESIGN §6a).
///
/// # Errors
/// Returns an error if the handler module cannot be read.
pub fn load_handler(
    store_root: &Path,
    scheme: &str,
    src_path: &str,
    caps: Capabilities,
) -> Result<(GeneratorSrc, HandlerRef)> {
    let path = store_root.join(src_path);
    let source = std::fs::read_to_string(&path)
        .map_err(|e| Error::Verifier(format!("reading {}: {e}", path.display())))?;
    let src = GeneratorSrc::new(scheme, source, caps.clone());
    let handler = HandlerRef {
        hash: src.hash(),
        caps,
        cost_estimate: src.cost_estimate,
        src_path: src_path.to_string(),
        scheme: scheme.to_string(),
    };
    Ok((src, handler))
}

/// The raw result of running a generator: its stdout (the produced value) and
/// enough to attribute a read failure.
#[derive(Debug, Clone)]
pub struct GeneratorRun {
    pub spawned: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub stderr_nonempty: bool,
    pub hash_changed: bool,
    pub stdout: String,
}

impl GeneratorRun {
    pub fn ok(&self) -> bool {
        self.spawned && !self.timed_out && self.exit_code == Some(0) && !self.hash_changed
    }
}

/// Capability-scoped Deno runner. Declared capabilities become sandbox flags;
/// nothing more is granted; a hard timeout always applies; no write-back.
#[derive(Debug)]
pub struct Sandbox {
    runtime: String,
    timeout: Duration,
}

impl Sandbox {
    pub fn new(runtime: impl Into<String>, timeout: Duration) -> Self {
        Sandbox {
            runtime: runtime.into(),
            timeout,
        }
    }

    pub fn available(&self) -> bool {
        Command::new(&self.runtime)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Run a generator with `KEN_VALUE` set to the claim. Its stdout is the
    /// produced ground value.
    pub fn run(
        &self,
        src: &GeneratorSrc,
        recorded_hash: Option<&GeneratorHash>,
        claim: &str,
    ) -> GeneratorRun {
        let current = src.hash();
        let hash_changed = recorded_hash.is_some_and(|h| *h != current);

        let script = std::env::temp_dir().join(format!("ken-gen-{}.ts", &current.0[..16]));
        let stderr_path =
            std::env::temp_dir().join(format!("ken-gen-err-{}.log", &current.0[..16]));
        let stdout_path =
            std::env::temp_dir().join(format!("ken-gen-out-{}.log", &current.0[..16]));
        if std::fs::write(&script, &src.source).is_err() {
            return errored(hash_changed);
        }
        let (Ok(stderr_file), Ok(stdout_file)) = (
            std::fs::File::create(&stderr_path),
            std::fs::File::create(&stdout_path),
        ) else {
            return errored(hash_changed);
        };

        let mut cmd = Command::new(&self.runtime);
        cmd.arg("run").arg("--no-prompt").arg("--quiet");
        cmd.arg(allow_env(&["KEN_VALUE"], &src.caps.env));
        if !src.caps.net.is_empty() {
            cmd.arg(format!("--allow-net={}", src.caps.net.join(",")));
        }
        if !src.caps.read.is_empty() {
            cmd.arg(format!("--allow-read={}", src.caps.read.join(",")));
        }
        cmd.arg(&script)
            .env("KEN_VALUE", claim)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));

        let Ok(mut child) = cmd.spawn() else {
            let _ = std::fs::remove_file(&script);
            return errored(hash_changed);
        };

        let start = Instant::now();
        let (timed_out, exit_code) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (false, status.code()),
                Ok(None) => {
                    if start.elapsed() >= self.timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        break (true, None);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => break (false, None),
            }
        };

        let stderr_nonempty = std::fs::metadata(&stderr_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&stderr_path);
        let _ = std::fs::remove_file(&stdout_path);

        GeneratorRun {
            spawned: true,
            timed_out,
            exit_code,
            stderr_nonempty,
            hash_changed,
            stdout,
        }
    }

    /// Run a scheme handler: a generated bootstrap imports the handler module's
    /// default export and calls `fetch(reference, env)`, where `reference` is
    /// the CURIE's path and `env` is the declared `env` capabilities resolved
    /// from the host. The handler's returned string is its stdout (the document
    /// bytes); a throw is a nonzero exit (`Errored`); a timeout is `Transient`.
    pub fn run_handler(
        &self,
        src: &GeneratorSrc,
        recorded_hash: Option<&GeneratorHash>,
        reference: &str,
        rev: Option<&str>,
    ) -> GeneratorRun {
        let current = src.hash();
        let hash_changed = recorded_hash.is_some_and(|h| *h != current);

        let stem = &current.0[..16];
        let dir = std::env::temp_dir();
        let module = dir.join(format!("ken-handler-{stem}.ts"));
        let entry = dir.join(format!("ken-handler-entry-{stem}.ts"));
        let stderr_path = dir.join(format!("ken-handler-err-{stem}.log"));
        let stdout_path = dir.join(format!("ken-handler-out-{stem}.log"));

        let module_name = module
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let env_keys: Vec<String> = src
            .caps
            .env
            .iter()
            .map(|k| serde_json::to_string(k).unwrap_or_default())
            .collect();
        let bootstrap = format!(
            "import handler from \"./{module_name}\";\n\
             const reference = Deno.env.get(\"KEN_REF\") ?? \"\";\n\
             const env: Record<string, string> = {{}};\n\
             for (const k of [{keys}]) {{ const v = Deno.env.get(k); if (v !== undefined) env[k] = v; }}\n\
             const out = await handler.fetch(reference, env);\n\
             const text = typeof out === \"string\" ? out : JSON.stringify(out);\n\
             await Deno.stdout.write(new TextEncoder().encode(text));\n",
            keys = env_keys.join(", "),
        );

        if std::fs::write(&module, &src.source).is_err()
            || std::fs::write(&entry, bootstrap).is_err()
        {
            let _ = std::fs::remove_file(&module);
            return errored(hash_changed);
        }
        let (Ok(stderr_file), Ok(stdout_file)) = (
            std::fs::File::create(&stderr_path),
            std::fs::File::create(&stdout_path),
        ) else {
            let _ = std::fs::remove_file(&module);
            let _ = std::fs::remove_file(&entry);
            return errored(hash_changed);
        };

        let mut cmd = Command::new(&self.runtime);
        cmd.arg("run").arg("--no-prompt").arg("--quiet");
        cmd.arg(allow_env(&["KEN_REF", "KEN_REV"], &src.caps.env));
        if !src.caps.net.is_empty() {
            cmd.arg(format!("--allow-net={}", src.caps.net.join(",")));
        }
        if !src.caps.read.is_empty() {
            cmd.arg(format!("--allow-read={}", src.caps.read.join(",")));
        }
        cmd.arg(&entry)
            .env("KEN_REF", reference)
            .env("KEN_REV", rev.unwrap_or(""))
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));

        let Ok(mut child) = cmd.spawn() else {
            let _ = std::fs::remove_file(&module);
            let _ = std::fs::remove_file(&entry);
            return errored(hash_changed);
        };

        let start = Instant::now();
        let (timed_out, exit_code) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (false, status.code()),
                Ok(None) => {
                    if start.elapsed() >= self.timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        break (true, None);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => break (false, None),
            }
        };

        let stderr_nonempty = std::fs::metadata(&stderr_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        let _ = std::fs::remove_file(&module);
        let _ = std::fs::remove_file(&entry);
        let _ = std::fs::remove_file(&stderr_path);
        let _ = std::fs::remove_file(&stdout_path);

        GeneratorRun {
            spawned: true,
            timed_out,
            exit_code,
            stderr_nonempty,
            hash_changed,
            stdout,
        }
    }
}

/// Build a `--allow-env` flag from a fixed set of ken-injected names plus the
/// declared `env` capability (e.g. an auth token a handler needs).
fn allow_env(base: &[&str], caps_env: &[String]) -> String {
    let mut names: Vec<String> = base.iter().map(|&s| s.to_string()).collect();
    names.extend(caps_env.iter().cloned());
    format!("--allow-env={}", names.join(","))
}

fn errored(hash_changed: bool) -> GeneratorRun {
    GeneratorRun {
        spawned: false,
        timed_out: false,
        exit_code: None,
        stderr_nonempty: true,
        hash_changed,
        stdout: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_covers_capabilities() {
        let net_free = GeneratorSrc::new("v.ts", "console.log('x')", Capabilities::default());
        let with_net = GeneratorSrc::new(
            "v.ts",
            "console.log('x')",
            Capabilities {
                net: vec!["example.com".into()],
                read: vec![],
                env: vec![],
            },
        );
        assert_ne!(net_free.hash(), with_net.hash());
    }

    #[test]
    fn net_generator_costs_more() {
        let net_free = GeneratorSrc::new("v.ts", "x", Capabilities::default());
        let with_net = GeneratorSrc::new(
            "v.ts",
            "x",
            Capabilities {
                net: vec!["a.com".into()],
                read: vec![],
                env: vec![],
            },
        );
        assert!(with_net.cost_estimate > net_free.cost_estimate);
    }

    #[test]
    fn handler_hash_covers_env_caps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("handlers")).unwrap();
        std::fs::write(
            dir.path().join("handlers/wiki.ts"),
            "export default { fetch: (r: string) => r };",
        )
        .unwrap();
        let (_, plain) = load_handler(
            dir.path(),
            "wiki",
            "handlers/wiki.ts",
            Capabilities::default(),
        )
        .unwrap();
        let (_, with_env) = load_handler(
            dir.path(),
            "wiki",
            "handlers/wiki.ts",
            Capabilities {
                net: vec![],
                read: vec![],
                env: vec!["WIKI_TOKEN".into()],
            },
        )
        .unwrap();
        assert_ne!(
            plain.hash, with_env.hash,
            "env caps must fold into the hash"
        );
        assert_eq!(with_env.scheme, "wiki");
        assert_eq!(with_env.src_path, "handlers/wiki.ts");
    }

    #[test]
    fn handler_resolves_reference_to_stdout() {
        let sb = Sandbox::new("deno", Duration::from_secs(10));
        if !sb.available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("handlers")).unwrap();
        std::fs::write(
            dir.path().join("handlers/echo.ts"),
            "export default { fetch(reference: string) { return `page:${reference}`; } };",
        )
        .unwrap();
        let (src, h) = load_handler(
            dir.path(),
            "echo",
            "handlers/echo.ts",
            Capabilities::default(),
        )
        .unwrap();
        let run = sb.run_handler(&src, Some(&h.hash), "Architecture", None);
        assert!(run.ok(), "handler should exit cleanly: {run:?}");
        assert_eq!(run.stdout, "page:Architecture");
    }

    #[test]
    fn generator_emits_stdout() {
        let sb = Sandbox::new("deno", Duration::from_secs(10));
        if !sb.available() {
            return;
        }
        let src = GeneratorSrc::new(
            "echo.ts",
            "console.log(JSON.stringify({owner: Deno.env.get('KEN_VALUE')}))",
            Capabilities::default(),
        );
        let run = sb.run(&src, Some(&src.hash()), "alice");
        assert!(run.ok());
        assert!(run.stdout.contains("alice"));
    }
}
