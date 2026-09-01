//! Store selection: the predicate a fleet view takes instead of a store
//! name. Labels are `bark::provenance` — mutable and non-unique BY DESIGN
//! — so a selector names a SET, and the answer owes coverage: "matched
//! nothing" has to be distinguishable from "nothing was searched".
//!
//! Grammar — one expression, comma-separated conjunction:
//!
//! ```text
//! *              every store
//! text           the NAME contains text  (`name=*text`)
//! key=value      the label equals value
//! key!=value     it does not
//! key=~regex     it matches regex, anchored at both ends
//! key!~regex     it does not
//! key=*text      it CONTAINS text, anywhere in the value
//! key!*text      it does not
//! ```
//!
//! A bare word is the name because that is what a person types when they
//! know which log they want and not how it is labelled. It is `=*` and
//! not equality: `[apache]` is asked of a fleet, where the store is
//! called `apache-access` on one host and `apache2` on the next. A word
//! carrying `=`, `~`, `!` or `*` is a mistyped operator rather than a
//! name, and is refused — read as a name it would answer "no stores" for
//! a search nobody ran.
//!
//! `=*` exists because "the store whose name has `apache` in it" is the
//! commonest thing anyone asks, and an anchored regex is a poor way to
//! say it: `name=~.*apache.*` turns a user's literal text into a pattern,
//! so a name with a `.` or a `+` in it matches things nobody asked for.
//! It is the store-side counterpart of `timber-filter`'s `--substring`
//! and the query document's `substring` predicate, which have always had
//! this and which store selection did not.
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
    Contains,
    NotContains,
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
            // Literal, and case-sensitive like every other operator here.
            Op::Contains => have.contains(&self.value),
            Op::NotContains => !have.contains(&self.value),
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

    /// Does this store satisfy every term? `fields` is the WHOLE manifest,
    /// plus a `name` where one is only implied by the path: labels, name,
    /// id and settings are all matchable.
    ///
    /// Nothing is held back on the grounds of being the wrong KIND of
    /// fact. A key label is unique and stable where a name is neither, but
    /// that is a difference in what a match GUARANTEES, not in what a
    /// caller is allowed to ask — and a rule that exists only because we
    /// sorted two facts into different boxes is a rule that annoys the
    /// person who wanted the other one. The constraint that does bite is
    /// elsewhere: a writer's lookup must land on exactly one store or
    /// none, and that is enforced where the writing happens.
    pub fn matches(&self, fields: &Map<String, Value>) -> bool {
        self.terms.iter().all(|t| {
            let have = fields.get(&t.key).map(stringify).unwrap_or_default();
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

/// Every operator a term may use, longest first — the order the parser
/// tries them in.
///
/// Public because a caller that BUILDS a term (the query document) has to
/// refuse an operator this build does not know. Formatting an unknown one
/// into a selector string does not fail: the parser finds a shorter
/// operator inside it and reads the remainder as part of the value, so
/// `=?` becomes `=` against `?value` and `!=X` becomes `!=` against
/// `Xvalue` — the second matching nearly everything.
pub const OPS: [&str; 6] = ["!=", "!~", "!*", "=~", "=*", "="];

fn parse_term(part: &str) -> anyhow::Result<Term> {
    let part = part.trim();
    if part.is_empty() {
        bail!("empty term in selector (a stray comma?)");
    }
    // Longest operator first: `!=` and `!~` before `=`, else `key!=v`
    // would parse as key `key!` — a term that matches nothing, silently.
    // `=*` before `=` for the same reason, or `name=*x` would be an
    // equality against the literal `*x`.
    let (key, op, value) = if let Some((k, v)) = part.split_once("!=") {
        (k, Op::Ne, v)
    } else if let Some((k, v)) = part.split_once("!~") {
        (k, Op::NotMatch, v)
    } else if let Some((k, v)) = part.split_once("!*") {
        (k, Op::NotContains, v)
    } else if let Some((k, v)) = part.split_once("=~") {
        (k, Op::Match, v)
    } else if let Some((k, v)) = part.split_once("=*") {
        (k, Op::Contains, v)
    } else if let Some((k, v)) = part.split_once('=') {
        (k, Op::Eq, v)
    } else if part.contains(['=', '~', '!', '*']) {
        // Something that LOOKS like an operator but is not one is a typo,
        // not a store called that. Read as a name it would answer "no
        // stores" for a search nobody ran — which is the reading the bare
        // word below would otherwise give it.
        bail!(
            "selector term {part:?} has no operator I know — one of {}",
            OPS.join(", ")
        );
    } else {
        ("name", Op::Contains, part)
    };
    let key = key.trim();
    if key.is_empty() {
        bail!("selector term {part:?} has no label name");
    }
    let value = unquote(value.trim());
    // An empty substring is contained in everything, so it says nothing —
    // and `key!=` already spells "declares this key". Two ways to say one
    // thing, one of which reads like a typo, is worse than a refusal.
    if matches!(op, Op::Contains | Op::NotContains) && value.is_empty() {
        bail!(
            "selector term {part:?} has an empty substring, which every value contains — \
             use `{}!=` for \"declares this label\"",
            key.trim()
        );
    }
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

/// Everything about a store a selector may match on: its manifest as it
/// stands, plus a `name` for a store that has not declared one — where the
/// path is all the name there is, `--select name=...` must still find it.
pub fn selectable(bark: &Map<String, Value>, handle: &str) -> Map<String, Value> {
    let mut fields = bark.clone();
    fields
        .entry("name".to_string())
        .or_insert_with(|| Value::String(handle.to_string()));
    fields
}

/// One store a selector matched: enough to open it, and the labels it
/// matched on so a caller can say WHY it matched.
#[derive(Debug)]
pub struct Match {
    /// The store's handle — its file name minus `.log`.
    pub handle: String,
    /// The backing directory holding the pair.
    pub dir: std::path::PathBuf,
    /// The store's name within that directory.
    pub name: String,
    pub labels: Map<String, Value>,
}

/// Every store in the given forests (or all configured ones) whose labels
/// satisfy `sel`, in a stable order.
///
/// Deliberately lighter than what `list` builds for a row: a readdir per
/// forest and ONE manifest read per store, with no index or follower
/// registry touched. Selection is a lookup, and a lookup that also parsed
/// every store's index would be too expensive to put in a writer's path —
/// which is where this is headed.
pub fn resolve(dirs: &[std::path::PathBuf], sel: &Selector) -> Vec<Match> {
    let mut out = Vec::new();
    for forest in crate::forest::forests_for_list(dirs) {
        if !forest.dir.is_dir() {
            continue;
        }
        for (handle, path) in crate::forest::scan_forest(&forest.dir) {
            let Ok((dir, name)) = crate::query::resolve_backing(&path) else {
                continue;
            };
            let bark = crate::bark::load(&dir, &name).unwrap_or_default();
            let fields = selectable(&bark, &handle);
            if sel.matches(&fields) {
                let labels = crate::bark::provenance(&bark);
                out.push(Match {
                    handle,
                    dir,
                    name,
                    labels,
                });
            }
        }
    }
    out.sort_by(|a, b| (a.dir.as_path(), a.name.as_str()).cmp(&(b.dir.as_path(), b.name.as_str())));
    out
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
    fn a_substring_is_a_literal_not_a_pattern() {
        // The gesture this exists for: "the store with apache in its
        // name". As an anchored regex that is `name=~.*apache.*`, which
        // turns whatever the user typed into a pattern.
        let l = labels(&[("name", "apache-error@web01")]);
        assert!(Selector::parse("name=*apache").unwrap().matches(&l));
        assert!(Selector::parse("name=*error@web").unwrap().matches(&l));
        assert!(!Selector::parse("name=*nginx").unwrap().matches(&l));
        assert!(Selector::parse("name!*nginx").unwrap().matches(&l));
        assert!(!Selector::parse("name!*apache").unwrap().matches(&l));

        // A LITERAL: metacharacters are themselves, which is the whole
        // point. `.` matches a dot, not any character.
        let dotted = labels(&[("name", "a.b")]);
        assert!(Selector::parse("name=*a.b").unwrap().matches(&dotted));
        assert!(!Selector::parse("name=*axb").unwrap().matches(&dotted));
        let axb = labels(&[("name", "axb")]);
        assert!(
            !Selector::parse("name=*a.b").unwrap().matches(&axb),
            "a dot is a dot, where a regex would have matched"
        );

        // `=*` is tried before `=`, or this is an equality against `*x`.
        assert!(Selector::parse("name=*apache").unwrap().matches(&l));
        assert!(!Selector::parse("name=apache").unwrap().matches(&l));

        // Empty contains everything, so it says nothing — and `key!=`
        // already means "declares this label".
        assert!(Selector::parse("name=*").is_err());
        assert!(Selector::parse("name!*").is_err());
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
        assert!(Selector::parse("=web01").is_err(), "no label name");
        // A word carrying an operator character but no operator: a typo,
        // and the one case the bare-word rule must not swallow.
        for typo in ["host~web01", "host!web01", "web*01", "!host"] {
            assert!(Selector::parse(typo).is_err(), "{typo} should be refused");
        }
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

    /// A bare word is the store's name, matched anywhere in it — the same
    /// thing `[apache]` means in timbersh, so a selector an operator
    /// tested there is the selector a declaration can hold.
    #[test]
    fn a_bare_word_is_the_name_matched_anywhere_in_it() {
        let l = labels(&[("name", "apache-access"), ("host", "web01")]);
        assert!(Selector::parse("apache").unwrap().matches(&l));
        assert!(Selector::parse("access").unwrap().matches(&l));
        assert!(!Selector::parse("nginx").unwrap().matches(&l));
        // Contains, not equality: the same word finds a store called
        // something else on the next host.
        assert!(Selector::parse("apache")
            .unwrap()
            .matches(&labels(&[("name", "apache2")])));
        // It is a term like any other, so it ANDs and it quotes.
        assert!(Selector::parse("apache,host=web01").unwrap().matches(&l));
        assert!(Selector::parse("\"apache-access\"").unwrap().matches(&l));
    }

    /// A bare word says `name`, so it is a LITERAL — the reason `=*`
    /// exists rather than a regex spelling of it.
    #[test]
    fn a_bare_word_is_not_read_as_a_pattern() {
        let l = labels(&[("name", "apacheXaccess")]);
        assert!(!Selector::parse("apache.access").unwrap().matches(&l));
        assert!(Selector::parse("apache.access")
            .unwrap()
            .matches(&labels(&[("name", "apache.access")])));
    }
}
