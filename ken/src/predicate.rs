//! The judge: a pure, deterministic, capability-free predicate over a resolved
//! span. A predicate cannot rot (same span -> same verdict), so a refute is
//! unambiguous: the span moved. All non-determinism lives in the source layer
//! (reading), never here.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NumOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

impl NumOp {
    fn apply(&self, lhs: f64, rhs: f64) -> bool {
        match self {
            NumOp::Lt => lhs < rhs,
            NumOp::Le => lhs <= rhs,
            NumOp::Gt => lhs > rhs,
            NumOp::Ge => lhs >= rhs,
            NumOp::Eq => (lhs - rhs).abs() < f64::EPSILON,
        }
    }

    fn parse(s: &str) -> Result<NumOp> {
        Ok(match s {
            "lt" | "<" => NumOp::Lt,
            "le" | "<=" => NumOp::Le,
            "gt" | ">" => NumOp::Gt,
            "ge" | ">=" => NumOp::Ge,
            "eq" | "==" | "=" => NumOp::Eq,
            other => return Err(Error::Config(format!("unknown numeric op: {other}"))),
        })
    }
}

/// A pure judgment over `(claim, span)`. Holds no capabilities and never spawns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "pred", rename_all = "lowercase")]
pub enum Predicate {
    /// The span resolved to something non-empty (tier-1 existence).
    #[default]
    Exists,
    /// The span equals `literal`, or the claim value if absent.
    Equals { literal: Option<String> },
    /// The span contains `literal`, or the claim value if absent.
    Contains { literal: Option<String> },
    /// The span matches a regular expression.
    Matches { regex: String },
    /// The span parses as a number and compares to `value`.
    Numeric { op: NumOp, value: f64 },
    /// Resolve an RFC 6901 JSON Pointer into the span (parsed as JSON), then
    /// judge the addressed value with `then`.
    JsonPointer {
        pointer: String,
        then: Box<Predicate>,
    },
}

impl Predicate {
    /// Evaluate against the claim value and the resolved span. Total and
    /// deterministic: any malformed input is `false`, never an error (shape is
    /// caught by [`Predicate::validate`] at ground time).
    pub fn eval(&self, claim: &str, span: &str) -> bool {
        match self {
            Predicate::Exists => !span.trim().is_empty(),
            Predicate::Equals { literal } => {
                span.trim() == literal.as_deref().unwrap_or(claim).trim()
            }
            Predicate::Contains { literal } => span.contains(literal.as_deref().unwrap_or(claim)),
            Predicate::Matches { regex } => regex::Regex::new(regex)
                .map(|re| re.is_match(span))
                .unwrap_or(false),
            Predicate::Numeric { op, value } => span
                .trim()
                .parse::<f64>()
                .map(|n| op.apply(n, *value))
                .unwrap_or(false),
            Predicate::JsonPointer { pointer, then } => {
                let Ok(json) = serde_json::from_str::<serde_json::Value>(span) else {
                    return false;
                };
                match json.pointer(pointer) {
                    Some(v) => then.eval(claim, &json_as_str(v)),
                    None => false,
                }
            }
        }
    }

    /// Check the predicate is well-formed (regex compiles, pointer is a valid
    /// RFC 6901 string, sub-predicates recurse). Run at `ken ground` time so a
    /// bad predicate can never surface as a runtime outcome.
    pub fn validate(&self) -> Result<()> {
        match self {
            Predicate::Matches { regex } => {
                regex::Regex::new(regex)
                    .map_err(|e| Error::Config(format!("invalid regex: {e}")))?;
                Ok(())
            }
            Predicate::JsonPointer { pointer, then } => {
                // RFC 6901: the empty string is the whole document; otherwise a
                // pointer is a sequence of `/`-prefixed reference tokens.
                if !pointer.is_empty() && !pointer.starts_with('/') {
                    return Err(Error::Config(format!(
                        "invalid JSON pointer (RFC 6901): {pointer:?} must be empty or start with '/'"
                    )));
                }
                then.validate()
            }
            _ => Ok(()),
        }
    }

    /// Parse the CLI mini-grammar: `exists | equals[:lit] | contains[:lit] |
    /// matches:<re> | num:<op>:<n> | ptr:<rfc6901>[:<sub>]`.
    pub fn parse(spec: &str) -> Result<Predicate> {
        let spec = spec.trim();
        let (head, rest) = match spec.split_once(':') {
            Some((h, r)) => (h, Some(r)),
            None => (spec, None),
        };
        let p = match head {
            "exists" => Predicate::Exists,
            "equals" => Predicate::Equals {
                literal: rest.map(|s| s.to_string()),
            },
            "contains" => Predicate::Contains {
                literal: rest.map(|s| s.to_string()),
            },
            "matches" => Predicate::Matches {
                regex: rest
                    .ok_or_else(|| Error::Config("matches needs a regex: matches:<re>".into()))?
                    .to_string(),
            },
            "num" => {
                let rest = rest
                    .ok_or_else(|| Error::Config("num needs op and value: num:<op>:<n>".into()))?;
                let (op, n) = rest
                    .split_once(':')
                    .ok_or_else(|| Error::Config("num needs op and value: num:<op>:<n>".into()))?;
                Predicate::Numeric {
                    op: NumOp::parse(op)?,
                    value: n
                        .trim()
                        .parse()
                        .map_err(|_| Error::Config(format!("bad number: {n}")))?,
                }
            }
            "ptr" => {
                let rest =
                    rest.ok_or_else(|| Error::Config("ptr needs a pointer: ptr:<rfc6901>".into()))?;
                // Split once: pointer up to the first `:`, remainder is the
                // sub-predicate (defaults to `exists`).
                let (pointer, sub) = match rest.split_once(':') {
                    Some((p, s)) => (p.to_string(), Predicate::parse(s)?),
                    None => (rest.to_string(), Predicate::Exists),
                };
                Predicate::JsonPointer {
                    pointer,
                    then: Box::new(sub),
                }
            }
            other => return Err(Error::Config(format!("unknown predicate: {other}"))),
        };
        p.validate()?;
        Ok(p)
    }

    /// Short human label for `why`/`recall`.
    pub fn label(&self) -> String {
        match self {
            Predicate::Exists => "exists".into(),
            Predicate::Equals { .. } => "equals".into(),
            Predicate::Contains { .. } => "contains".into(),
            Predicate::Matches { .. } => "matches".into(),
            Predicate::Numeric { op, value } => format!("num {op:?} {value}"),
            Predicate::JsonPointer { pointer, then } => format!("ptr {pointer} {}", then.label()),
        }
    }
}

/// Render a JSON value as the string a sub-predicate judges: strings unquoted,
/// everything else compact JSON.
fn json_as_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exists_checks_nonempty() {
        assert!(Predicate::Exists.eval("c", "something"));
        assert!(!Predicate::Exists.eval("c", "   "));
    }

    #[test]
    fn equals_vs_claim_or_literal() {
        assert!(Predicate::Equals { literal: None }.eval("hello", "hello"));
        assert!(!Predicate::Equals { literal: None }.eval("hello", "world"));
        assert!(Predicate::Equals {
            literal: Some("x".into())
        }
        .eval("hello", "x"));
    }

    #[test]
    fn contains_and_matches() {
        assert!(Predicate::Contains { literal: None }.eval("auth", "auth lives here"));
        assert!(Predicate::Matches {
            regex: r"^\d{3}$".into()
        }
        .eval("c", "200"));
        assert!(!Predicate::Matches {
            regex: r"^\d{3}$".into()
        }
        .eval("c", "two hundred"));
    }

    #[test]
    fn numeric_compares() {
        assert!(Predicate::Numeric {
            op: NumOp::Ge,
            value: 100.0
        }
        .eval("c", "200"));
        assert!(!Predicate::Numeric {
            op: NumOp::Lt,
            value: 100.0
        }
        .eval("c", "200"));
        // Non-numeric span is false, not an error.
        assert!(!Predicate::Numeric {
            op: NumOp::Eq,
            value: 1.0
        }
        .eval("c", "abc"));
    }

    #[test]
    fn json_pointer_resolves_rfc6901() {
        let span = r#"{"owner": "alice", "nested": {"port": 8080}}"#;
        let p = Predicate::parse("ptr:/owner:equals").unwrap();
        assert!(p.eval("alice", span));
        assert!(!p.eval("bob", span));

        let port = Predicate::parse("ptr:/nested/port:num:==:8080").unwrap();
        assert!(port.eval("c", span));

        // Unresolved pointer fails (does not error).
        let missing = Predicate::parse("ptr:/absent:exists").unwrap();
        assert!(!missing.eval("c", span));
    }

    #[test]
    fn parse_and_validate() {
        assert_eq!(Predicate::parse("exists").unwrap(), Predicate::Exists);
        assert!(
            Predicate::parse("equals:http://x").unwrap()
                == Predicate::Equals {
                    literal: Some("http://x".into())
                }
        );
        // A bad regex is rejected at parse/validate time, never at eval time.
        assert!(Predicate::parse("matches:(").is_err());
        // A malformed pointer is rejected.
        assert!(Predicate::parse("ptr:owner:exists").is_err());
        assert!(Predicate::parse("nonsense").is_err());
    }
}
