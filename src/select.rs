//! Store selection: the predicate a fleet view takes instead of a store
//! name. Labels are `bark::provenance` — mutable and non-unique BY DESIGN
//! — so a selector names a SET, and the answer owes coverage: "matched
//! nothing" has to be distinguishable from "nothing was searched".
//!
//! Grammar — one expression, comma-separated conjunction:
//!
//! ```text
//! *              every store
//! key=value      the label equals value
//! key!=value     it does not
//! key=~regex     it matches regex, anchored at both ends
//! key!~regex     it does not
//! ```
//!
//! An absent label reads as the empty string, so `key!=` selects the
//! stores that declare `key` and `key=` the ones that do not — the same
//! rule Prometheus and Loki use, and one vocabulary end to end (see
//! docs/plans/receiving-end.md). A value that must contain a comma is
//! double-quoted: `service=~"a{1,2}"`.

use anyhow::{bail, Context};
use regex::Regex;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Match,
    NotMatch,
}

#[derive(Debug)]
struct Term {
    key: String,
    op: Op,
    value: String,
    re: Option<Regex>,
}

impl Term {
    fn matches(&self, have: &str) -> bool {
        match self.op {
            Op::Eq => have == self.value,
            Op::Ne => have != self.value,
            // Parsing built the regex, so `re` is Some for both regex ops.
            Op::Match => self.re.as_ref().is_some_and(|r| r.is_match(have)),
            Op::NotMatch => self.re.as_ref().is_some_and(|r| !r.is_match(have)),
        }
    }
}

/// A parsed selector. `Selector::all()` is the `*` that matches every
/// store, which is also what an omitted `--select` means.
#[derive(Debug)]
pub struct Selector {
    terms: Vec<Term>,
}

impl Selector {
    /// The selector that matches every store.
    pub fn all() -> Self {
        Selector { terms: Vec::new() }
    }

    pub fn parse(expr: &str) -> anyhow::Result<Self> {
        let expr = expr.trim();
        if expr.is_empty() {
            bail!("empty selector: use `*` to select every store");
        }
        if expr == "*" {
            return Ok(Selector::all());
        }
        let mut terms = Vec::new();
        for part in split_terms(expr)? {
            terms.push(parse_term(&part)?);
        }
        Ok(Selector { terms })
    }

    /// Does this store's labels satisfy every term? `labels` is a
    /// manifest's provenance, never the whole manifest — selecting on an
    /// operational setting is what `bark::NOT_PROVENANCE` exists to stop.
    pub fn matches(&self, labels: &Map<String, Value>) -> bool {
        self.terms.iter().all(|t| {
            let have = labels.get(&t.key).map(stringify).unwrap_or_default();
            t.matches(&have)
        })
    }

    /// True when this selector is `*`, so a caller can say "every store"
    /// rather than echoing a predicate nobody typed.
    pub fn is_all(&self) -> bool {
        self.terms.is_empty()
    }
}

/// A label's value as text. Free-form keys may hold a non-string, and a
/// selector compares text, so render one the same way `info` does rather
/// than refusing to match it.
pub(crate) fn stringify(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

/// Split on commas that are not inside a double-quoted value, so a regex
/// may contain one. An unterminated quote is refused rather than read to
/// end of input, where the missing quote would silently swallow the terms
/// after it.
fn split_terms(expr: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in expr.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if quoted {
        bail!("unterminated `\"` in selector {expr:?}");
    }
    out.push(cur);
    Ok(out)
}

fn parse_term(part: &str) -> anyhow::Result<Term> {
    let part = part.trim();
    if part.is_empty() {
        bail!("empty term in selector (a stray comma?)");
    }
    // Longest operator first: `!=` and `!~` before `=`, else `key!=v`
    // would parse as key `key!` — a term that matches nothing, silently.
    let (key, op, value) = if let Some((k, v)) = part.split_once("!=") {
        (k, Op::Ne, v)
    } else if let Some((k, v)) = part.split_once("!~") {
        (k, Op::NotMatch, v)
    } else if let Some((k, v)) = part.split_once("=~") {
        (k, Op::Match, v)
    } else if let Some((k, v)) = part.split_once('=') {
        (k, Op::Eq, v)
    } else {
        bail!(
            "selector term {part:?} has no operator — expected key=value, \
             key!=value, key=~regex or key!~regex"
        );
    };
    let key = key.trim();
    if key.is_empty() {
        bail!("selector term {part:?} has no label name");
    }
    let value = unquote(value.trim());
    let re = match op {
        Op::Match | Op::NotMatch => Some(
            // Anchored, as in Prometheus and Loki: an unanchored `=~`
            // would make `service=~api` match `api-gateway` too, which is
            // the wrong kind of surprise in a fleet-wide predicate.
            Regex::new(&format!("^(?:{value})$"))
                .with_context(|| format!("selector term {part:?}: invalid regex"))?,
        ),
        _ => None,
    };
    Ok(Term {
        key: key.to_string(),
        op,
        value,
        re,
    })
}

fn unquote(v: &str) -> String {
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    #[test]
    fn star_and_an_omitted_selector_are_the_same_thing() {
        let l = labels(&[("host", "web01")]);
        assert!(Selector::parse("*").unwrap().matches(&l));
        assert!(Selector::all().matches(&l));
        assert!(Selector::parse("*").unwrap().is_all());
        // ...and both match a store with no labels at all, which is the
        // only answer that lets `list` keep listing an unlabelled store.
        assert!(Selector::all().matches(&Map::new()));
    }

    #[test]
    fn terms_are_anded_and_an_absent_label_is_the_empty_string() {
        let l = labels(&[("type", "console"), ("host", "sourcream")]);
        assert!(Selector::parse("type=console,host=sourcream")
            .unwrap()
            .matches(&l));
        assert!(!Selector::parse("type=console,host=other")
            .unwrap()
            .matches(&l));
        // Absent reads as empty: `=` selects the stores that LACK the key
        // and `!=` the ones that declare it, so "has a service" is
        // expressible without a second operator.
        assert!(Selector::parse("service=").unwrap().matches(&l));
        assert!(!Selector::parse("service!=").unwrap().matches(&l));
        assert!(Selector::parse("type!=").unwrap().matches(&l));
    }

    #[test]
    fn a_regex_is_anchored_at_both_ends() {
        let l = labels(&[("service", "api-gateway")]);
        assert!(Selector::parse("service=~api-.*").unwrap().matches(&l));
        assert!(Selector::parse("service=~.*gateway").unwrap().matches(&l));
        // Unanchored, this would match — and a fleet-wide predicate that
        // quietly widens is the failure this anchoring exists to stop.
        assert!(!Selector::parse("service=~api").unwrap().matches(&l));
        assert!(Selector::parse("service!~api").unwrap().matches(&l));
    }

    #[test]
    fn a_quoted_value_may_contain_the_separator() {
        let l = labels(&[("service", "aa")]);
        let s = Selector::parse(r#"service=~"a{1,2}""#).unwrap();
        assert!(s.matches(&l));
        // Unquoted, the comma would have split the term in two, and the
        // half-regex would have matched nothing rather than erroring.
        assert!(Selector::parse("service=~a{1,2}").is_err());
    }

    #[test]
    fn a_negated_operator_is_not_read_as_part_of_the_key() {
        // `key!=v` split on `=` first would yield the key `key!`, which
        // matches nothing and says nothing — wrong, silently.
        let l = labels(&[("host", "web01")]);
        assert!(Selector::parse("host!=web02").unwrap().matches(&l));
        assert!(!Selector::parse("host!=web01").unwrap().matches(&l));
    }

    #[test]
    fn malformed_selectors_are_refused_not_guessed() {
        assert!(Selector::parse("").is_err());
        assert!(Selector::parse("host").is_err(), "no operator");
        assert!(Selector::parse("=web01").is_err(), "no label name");
        assert!(Selector::parse("host=web01,").is_err(), "stray comma");
        assert!(
            Selector::parse(r#"host=~"web.*"#).is_err(),
            "unterminated quote"
        );
        assert!(Selector::parse("host=~[").is_err(), "invalid regex");
    }

    #[test]
    fn a_non_string_label_compares_as_the_text_info_would_print() {
        let mut l = Map::new();
        l.insert("replicas".to_string(), Value::from(3));
        assert!(Selector::parse("replicas=3").unwrap().matches(&l));
    }
}
