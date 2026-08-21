//! Authoring-intent sidecars for aliases and triggers, and the compilers that
//! reduce them to the regexes the runtime actually matches.
//!
//! The persisted `pattern` / `patterns` / `raw_patterns` / `anti_patterns`
//! fields remain the compiled regex and remain authoritative for the runtime.
//! The sidecar records *how the author wrote* a matcher — a Simple pattern
//! with its anchor checkboxes, or a Command with its argument rows — and the
//! stored regex is recompiled from it on every save, never on load. An absent
//! sidecar means the matcher is a hand-written regex shown verbatim, which is
//! exactly the pre-sidecar behavior, so older files load unchanged and older
//! clients ignore the field.
//!
//! Pattern holes follow `TinTin++` semantics: `{name}` is a lazy wildcard
//! (greedy when it ends the pattern), `{name...}` is the greedy
//! rest-of-line form, and suffixes narrow a hole (`:word`, `:number`,
//! `:rest`). Holes number by position — named or anonymous (`{}`) — exactly
//! like regex groups; there is no `{1}` syntax, and an all-digit body is a
//! compile error rather than silent literal text. Compilation is
//! case-sensitive, like every other matcher in smudgy.

use regex::Regex;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

// serde's `skip_serializing_if` requires a `fn(&T) -> bool`.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(value: &bool) -> bool {
    *value
}

/// Which runtime vector a trigger matcher lands in. `Anti` is the model and
/// on-disk word (matching `anti_patterns` / `antiPatterns`); the UI renders it
/// as "Exceptions".
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MatcherRole {
    Match,
    Anti,
    Raw,
}

/// How a matcher's source is written. `Raw`-role matchers are always `Regex`
/// syntax (the editor enforces it).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MatcherSyntax {
    Pattern,
    Regex,
}

/// Which characters may group a multi-word Command argument into one token.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ParseMode {
    Spaces,
    Quotes,
    Braces,
    #[default]
    All,
    Raw,
}

/// Whether the Command editor showed its Simple or Advanced face. Editor
/// state only — both modes produce the same runtime shape.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CmdMode {
    #[default]
    Simple,
    Advanced,
}

/// How one Command argument consumes tokens. `Rest` may only appear in the
/// last position (the editor enforces it).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArgKind {
    Required,
    Optional,
    Rest,
}

/// One Command argument row.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: String,
    pub kind: ArgKind,
}

/// The authoring intent behind an alias's stored `pattern`. A hand-written
/// regex has no sidecar at all, so there is no `Regex` variant.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AliasMatcherSource {
    Command {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<ArgSpec>,
        #[serde(default)]
        parse: ParseMode,
        #[serde(default)]
        mode: CmdMode,
    },
    Pattern {
        source: String,
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        anchor_start: bool,
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        anchor_end: bool,
    },
}

/// The authoring intent behind one trigger matcher row. The sidecar records
/// the whole row list in author order and the three runtime vectors are
/// derived from it on save — never aligned positionally against them, which
/// would be fragile under reorder.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TriggerMatcherSource {
    pub role: MatcherRole,
    pub syntax: MatcherSyntax,
    pub source: String,
    /// Pattern syntax only; a Regex row declares its own anchors via `^`/`$`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub anchor_start: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub anchor_end: bool,
}

/// A pattern-compilation error. These are typed so the editor can render
/// localized strings; the engine message in [`PatternError::Engine`] is
/// surfaced verbatim, untranslated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    /// An all-digit hole body (`{1}`, `{0}`): holes number by position, so
    /// numbered holes do not exist — and coming from `%1`-style clients this
    /// is a likely typo that must not silently compile as literal text.
    NumberedHole { body: String },
    /// An identifier hole with an unrecognized `:type` suffix (`{hp:frog}`).
    /// Reserved rather than literal for the same silent-typo reason.
    UnknownHoleType { body: String },
    /// The compiled source was rejected by the regex engine (an island using
    /// unsupported syntax, a duplicated hole name, …).
    Engine { message: String },
}

/// A non-blocking pattern warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternWarning {
    /// Nothing in the pattern requires any concrete character, so it matches
    /// every line (the empty line included). Legal — equivalent regexes were
    /// always writable — but worth a warning on a construct this quiet.
    MatchesEveryLine,
}

/// The result of compiling one Simple pattern.
#[derive(Debug)]
pub struct CompiledPattern {
    /// The regex source, anchors applied — exactly what an alias/trigger
    /// stores. Meaningless when `errors` is non-empty.
    pub source: String,
    /// One entry per capture group in order (group 1 first): the hole's name,
    /// or `None` for an anonymous hole.
    pub captures: Vec<Option<String>>,
    /// The compiled regex; `None` whenever `errors` is non-empty.
    pub regex: Option<Regex>,
    pub errors: Vec<PatternError>,
    pub warnings: Vec<PatternWarning>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HoleKind {
    /// `{name}` / `{}` — the `TinTin` wildcard: lazy, greedy at end of pattern.
    Wild,
    /// `:word`, or any `?`-optional hole without a narrower type — exactly one
    /// word. (An optional wildcard would be meaningless: `.*?` already
    /// matches empty.)
    Word,
    /// `:number`.
    Number,
    /// `{name...}` / `:rest` — greedy, at least one character.
    Rest,
}

/// The characters escaped in literal pattern text.
fn escape_into(c: char, out: &mut String) {
    if matches!(
        c,
        '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' | '/'
    ) {
        out.push('\\');
    }
    out.push(c);
}

/// Emit literal text with the pattern language's whitespace rule (any run of
/// whitespace matches any run of whitespace).
fn emit_literal(text: &str, out: &mut String) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            out.push_str("\\s+");
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
        } else {
            escape_into(c, out);
        }
    }
}

/// Splits a hole body into `name[:type]`, returning `None` when the body is
/// not identifier-shaped (which makes the braces literal text).
fn identifier_body(body: &str) -> Option<(&str, Option<&str>)> {
    let (name, suffix) = match body.split_once(':') {
        Some((name, suffix)) => (name, Some(suffix)),
        None => (body, None),
    };
    let mut chars = name.chars();
    let leading_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !leading_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    if let Some(suffix) = suffix
        && (suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return None;
    }
    Some((name, suffix))
}

/// Compiles one Simple pattern to regex source. See the module docs for the
/// grammar; `fixtures.md` in the design plan is the behavioral contract and
/// the test suite below mirrors it.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn compile_pattern(source: &str, anchor_start: bool, anchor_end: bool) -> CompiledPattern {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut captures: Vec<Option<String>> = Vec::new();
    let mut errors: Vec<PatternError> = Vec::new();
    let mut warnings = Vec::new();
    // Whether anything emitted so far requires at least one concrete
    // character on the subject line; drives the matches-every-line warning.
    let mut has_required = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c == '{' {
            let Some(end) = chars[i + 1..]
                .iter()
                .position(|&c| c == '}')
                .map(|p| p + i + 1)
            else {
                // No closing brace: a literal `{`.
                out.push_str("\\{");
                i += 1;
                continue;
            };
            let inner: String = chars[i + 1..end].iter().collect();
            i = end + 1;

            let mut body = inner.trim().to_string();
            let mut kind = HoleKind::Wild;
            let mut optional = false;
            if body.ends_with("...") {
                kind = HoleKind::Rest;
                body.truncate(body.len() - 3);
            }
            if body.ends_with('?') {
                optional = true;
                body.pop();
            }
            if optional && kind == HoleKind::Wild {
                kind = HoleKind::Word;
            }

            let name = if body.is_empty() {
                None
            } else if body.chars().all(|c| c.is_ascii_digit()) {
                errors.push(PatternError::NumberedHole { body });
                continue;
            } else if let Some((name, suffix)) = identifier_body(&body) {
                match suffix {
                    None => {}
                    Some("word") => kind = HoleKind::Word,
                    Some("number") => kind = HoleKind::Number,
                    Some("rest") => kind = HoleKind::Rest,
                    Some(_) => {
                        errors.push(PatternError::UnknownHoleType { body: body.clone() });
                        continue;
                    }
                }
                Some(name.to_string())
            } else {
                // Not a hole: the braces and their original text are literal.
                out.push_str("\\{");
                emit_literal(&inner, &mut out);
                out.push_str("\\}");
                has_required = true;
                continue;
            };

            let group = match kind {
                // The TinTin rule: greedy when nothing follows the hole.
                HoleKind::Wild => {
                    if i >= chars.len() {
                        ".*"
                    } else {
                        ".*?"
                    }
                }
                HoleKind::Word => "\\S+",
                HoleKind::Number => "-?\\d+(?:\\.\\d+)?",
                HoleKind::Rest => ".+",
            };
            let open = match &name {
                Some(name) => format!("(?<{name}>"),
                None => "(".to_string(),
            };
            captures.push(name);
            if optional && out.ends_with("\\s+") {
                // Fold the preceding space into the optional group, so an
                // absent optional does not demand a trailing space.
                out.truncate(out.len() - 3);
                out.push_str("(?:\\s+");
                out.push_str(&open);
                out.push_str(group);
                out.push_str("))?");
            } else {
                out.push_str(&open);
                out.push_str(group);
                out.push(')');
                if optional {
                    out.push('?');
                }
            }
            if !optional && !matches!(kind, HoleKind::Wild) {
                has_required = true;
            }
            continue;
        }

        if c == '/' {
            // A /…/ island: raw regex inserted verbatim, grouped so an island
            // containing `|` cannot hijack the surrounding pattern.
            let mut j = i + 1;
            let mut found = None;
            while j < chars.len() {
                match chars[j] {
                    '\\' => j += 2,
                    '/' => {
                        found = Some(j);
                        break;
                    }
                    _ => j += 1,
                }
            }
            if let Some(close) = found {
                out.push_str("(?:");
                out.extend(&chars[i + 1..close]);
                out.push(')');
                // An island's requirements are unknowable without parsing it;
                // treat it as required text rather than second-guessing.
                has_required = true;
                i = close + 1;
            } else {
                // An unpaired slash is a literal slash.
                out.push_str("\\/");
                has_required = true;
                i += 1;
            }
            continue;
        }

        if c == '*' {
            out.push_str(".*");
            i += 1;
            continue;
        }

        if c.is_whitespace() {
            out.push_str("\\s+");
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            has_required = true;
            continue;
        }

        escape_into(c, &mut out);
        has_required = true;
        i += 1;
    }

    if !has_required {
        warnings.push(PatternWarning::MatchesEveryLine);
    }

    let mut source = String::with_capacity(out.len() + 2);
    if anchor_start {
        source.push('^');
    }
    source.push_str(&out);
    if anchor_end {
        source.push('$');
    }

    let regex = if errors.is_empty() {
        match Regex::new(&source) {
            Ok(regex) => Some(regex),
            Err(err) => {
                errors.push(PatternError::Engine {
                    message: err.to_string(),
                });
                None
            }
        }
    } else {
        None
    };

    CompiledPattern {
        source,
        captures,
        regex,
        errors,
        warnings,
    }
}

/// Translates `\e` (an ESC-byte convenience with no regex-crate equivalent)
/// to `\x1b` in a Raw matcher's source. Escape-aware: `\\e` is a literal
/// backslash followed by `e` and passes through unchanged. Applied only to
/// `Raw`-role sources, never to normal regex or pattern text.
#[must_use]
pub fn translate_esc(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('e') => out.push_str("\\x1b"),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The regex a Command alias stores: a prefilter selecting lines whose first
/// whitespace-delimited word is exactly `name`, case-sensitively. Not
/// `^name\b` — a word boundary fails for the punctuation names players
/// actually use (`*`, `#`, `@`) and wrongly matches `greet-me`. Firing and
/// captures are decided by the argument parser, not this regex.
#[must_use]
pub fn command_prefilter(name: &str) -> String {
    format!("^{}(?:\\s|$)", regex::escape(name))
}

/// The stored `pattern` derived from an alias sidecar — what a save writes.
///
/// # Errors
///
/// Returns the pattern's compile errors; a Command prefilter cannot fail.
pub fn alias_pattern(matcher: &AliasMatcherSource) -> Result<String, Vec<PatternError>> {
    match matcher {
        AliasMatcherSource::Command { name, .. } => Ok(command_prefilter(name)),
        AliasMatcherSource::Pattern {
            source,
            anchor_start,
            anchor_end,
        } => {
            let compiled = compile_pattern(source, *anchor_start, *anchor_end);
            if compiled.errors.is_empty() {
                Ok(compiled.source)
            } else {
                Err(compiled.errors)
            }
        }
    }
}

/// The three runtime vectors derived from a trigger's sidecar rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DerivedTriggerPatterns {
    pub patterns: Vec<String>,
    pub raw_patterns: Vec<String>,
    pub anti_patterns: Vec<String>,
}

/// Derives the stored `patterns` / `raw_patterns` / `anti_patterns` from a
/// trigger's sidecar rows — what a save writes. Pattern rows compile; Regex
/// rows are stored verbatim except that `Raw`-role sources get the `\e`
/// translation (the stored form must be valid regex; the sidecar keeps what
/// the author wrote). Every row is validated against the engine.
///
/// # Errors
///
/// Returns `(row index, error)` for each row that fails to compile.
pub fn trigger_patterns(
    matchers: &[TriggerMatcherSource],
) -> Result<DerivedTriggerPatterns, Vec<(usize, PatternError)>> {
    let mut derived = DerivedTriggerPatterns::default();
    let mut errors = Vec::new();

    for (index, matcher) in matchers.iter().enumerate() {
        let compiled = match matcher.syntax {
            MatcherSyntax::Pattern => {
                let compiled =
                    compile_pattern(&matcher.source, matcher.anchor_start, matcher.anchor_end);
                if compiled.errors.is_empty() {
                    compiled.source
                } else {
                    errors.extend(compiled.errors.into_iter().map(|e| (index, e)));
                    continue;
                }
            }
            MatcherSyntax::Regex => {
                let source = if matcher.role == MatcherRole::Raw {
                    translate_esc(&matcher.source)
                } else {
                    matcher.source.clone()
                };
                if let Err(err) = Regex::new(&source) {
                    errors.push((
                        index,
                        PatternError::Engine {
                            message: err.to_string(),
                        },
                    ));
                    continue;
                }
                source
            }
        };
        match matcher.role {
            MatcherRole::Match => derived.patterns.push(compiled),
            MatcherRole::Anti => derived.anti_patterns.push(compiled),
            MatcherRole::Raw => derived.raw_patterns.push(compiled),
        }
    }

    if errors.is_empty() {
        Ok(derived)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile with both anchors on (the default) and expect success.
    fn compiled(source: &str) -> CompiledPattern {
        let result = compile_pattern(source, true, true);
        assert!(
            result.errors.is_empty(),
            "{source:?} failed to compile: {:?}",
            result.errors
        );
        result
    }

    mod compilation {
        use super::*;

        /// fixtures.md §3 — the exact stored source per pattern.
        #[test]
        fn fixture_shapes() {
            for (pattern, expected) in [
                ("greet {person}", r"^greet\s+(?<person>.*)$"),
                (
                    "greet {person} warmly",
                    r"^greet\s+(?<person>.*?)\s+warmly$",
                ),
                ("greet {person...}", r"^greet\s+(?<person>.+)$"),
                ("greet {person?}", r"^greet(?:\s+(?<person>\S+))?$"),
                ("you gain {n} exp", r"^you\s+gain\s+(?<n>.*?)\s+exp$"),
                (
                    "you gain {n:number} exp",
                    r"^you\s+gain\s+(?<n>-?\d+(?:\.\d+)?)\s+exp$",
                ),
                ("grab {item:word}", r"^grab\s+(?<item>\S+)$"),
                ("hit {} for {}", r"^hit\s+(.*?)\s+for\s+(.*)$"),
                ("say {...}", r"^say\s+(.+)$"),
                ("greet {?}", r"^greet(?:\s+(\S+))?$"),
                ("you are *", r"^you\s+are\s+.*$"),
                (r"/\s*/{person} bows", r"^(?:\s*)(?<person>.*?)\s+bows$"),
                ("cost: 5/10 gold", r"^cost:\s+5\/10\s+gold$"),
                ("a.b (c)", r"^a\.b\s+\(c\)$"),
                ("greet {not a name}", r"^greet\s+\{not\s+a\s+name\}$"),
                ("greet {person", r"^greet\s+\{person$"),
            ] {
                assert_eq!(compiled(pattern).source, expected, "pattern: {pattern:?}");
            }
        }

        #[test]
        fn anchors_off_and_the_lazy_greedy_split() {
            let result = compile_pattern("{a} {b}", false, false);
            assert_eq!(result.source, r"(?<a>.*?)\s+(?<b>.*)");
            assert!(result.warnings.is_empty(), "\\s+ is required text");

            let result = compile_pattern("{x}", false, false);
            assert_eq!(result.source, r"(?<x>.*)");
            assert_eq!(result.warnings, vec![PatternWarning::MatchesEveryLine]);
        }

        #[test]
        fn all_digit_holes_are_errors() {
            for pattern in ["hit {1}", "hit {0}", "hit {17}"] {
                let result = compile_pattern(pattern, true, true);
                assert!(
                    matches!(
                        result.errors.as_slice(),
                        [PatternError::NumberedHole { .. }]
                    ),
                    "{pattern:?}: {:?}",
                    result.errors
                );
            }
        }

        #[test]
        fn unknown_hole_type_is_an_error_not_literal_text() {
            let result = compile_pattern("hp {hp:frog}", true, true);
            assert_eq!(
                result.errors,
                vec![PatternError::UnknownHoleType {
                    body: "hp:frog".to_string()
                }]
            );
        }

        #[test]
        fn capture_list_is_positional_with_anonymous_gaps() {
            let result = compiled("hit {} for {dmg} at {}");
            assert_eq!(result.captures, vec![None, Some("dmg".to_string()), None]);
        }

        #[test]
        fn island_engine_errors_surface_verbatim() {
            // Lookahead is not supported by the engine; the island's failure
            // must surface as an engine error, untranslated.
            let result = compile_pattern("/(?=a)/x", true, true);
            assert!(
                matches!(result.errors.as_slice(), [PatternError::Engine { .. }]),
                "{:?}",
                result.errors
            );
        }

        #[test]
        fn duplicate_hole_names_surface_the_engine_error() {
            let result = compile_pattern("{a} and {a}", true, true);
            assert!(
                matches!(result.errors.as_slice(), [PatternError::Engine { .. }]),
                "{:?}",
                result.errors
            );
        }
    }

    mod matching {
        use super::*;

        /// Assert one fixtures.md §4 row: `Some(pairs)` = fires with exactly
        /// these named captures, `None` = does not match.
        fn check(
            pattern: &str,
            anchors: (bool, bool),
            input: &str,
            expect: Option<&[(&str, &str)]>,
        ) {
            let result = compile_pattern(pattern, anchors.0, anchors.1);
            assert!(result.errors.is_empty(), "{pattern:?}: {:?}", result.errors);
            let regex = result.regex.expect("compiled");
            match (regex.captures(input), expect) {
                (None, None) => {}
                (Some(captures), Some(pairs)) => {
                    for (name, value) in pairs {
                        assert_eq!(
                            captures.name(name).map(|m| m.as_str()),
                            Some(*value),
                            "{pattern:?} vs {input:?}: capture {name:?}"
                        );
                    }
                }
                (got, _) => panic!(
                    "{pattern:?} vs {input:?}: expected {}, got {}",
                    if expect.is_some() {
                        "a match"
                    } else {
                        "no match"
                    },
                    if got.is_some() { "a match" } else { "no match" },
                ),
            }
        }

        const ON: (bool, bool) = (true, true);
        const OFF: (bool, bool) = (false, false);

        #[test]
        fn fixture_rows() {
            check(
                "greet {person} warmly",
                ON,
                "greet Mira warmly",
                Some(&[("person", "Mira")]),
            );
            // A hole spans words.
            check(
                "greet {person} warmly",
                ON,
                "greet Mira Bob warmly",
                Some(&[("person", "Mira Bob")]),
            );
            check("greet {person} warmly", ON, "greet Mira", None);
            check(
                "greet {person} warmly",
                ON,
                "Bob says greet Mira warmly",
                None,
            );
            check(
                "greet {person} warmly",
                OFF,
                "Bob says greet Mira warmly now",
                Some(&[("person", "Mira")]),
            );
            check(
                "You are {state}.",
                ON,
                "You are hungry.",
                Some(&[("state", "hungry")]),
            );
            // The D9b headline case: TinTin wildcard semantics.
            check(
                "You are {state}.",
                ON,
                "You are very hungry.",
                Some(&[("state", "very hungry")]),
            );
            // Case-sensitive (D12).
            check("You are {state}.", ON, "you are HUNGRY.", None);
            check(
                "You are {state...}",
                ON,
                "You are very hungry.",
                Some(&[("state", "very hungry.")]),
            );
            check(
                "{person} hits you",
                OFF,
                "The orc hits you hard",
                Some(&[("person", "The orc")]),
            );
            // A hole may match empty; empty is distinct from absent.
            check("greet {person}", ON, "greet ", Some(&[("person", "")]));
            check("greet {person}", ON, "greet", None);
            // `:word` is exactly one word.
            check("greet {person:word}", ON, "greet Mira Bob", None);
            check(
                "you gain {n:number} exp",
                ON,
                "you gain 42 exp",
                Some(&[("n", "42")]),
            );
            check("you gain {n:number} exp", ON, "you gain some exp", None);
            // Adjacent holes: lazy first, greedy last.
            check(
                "{a} {b}",
                OFF,
                "one two three",
                Some(&[("a", "one"), ("b", "two three")]),
            );
            // A bare hole matches every line, the empty line included.
            check("{x}", OFF, "", Some(&[("x", "")]));
            check("you are *", ON, "you are hungry and tired", Some(&[]));
            check("greet {person?}", ON, "greet", Some(&[]));
            check(
                "greet {person?}",
                ON,
                "greet Mira",
                Some(&[("person", "Mira")]),
            );
        }

        #[test]
        fn anonymous_holes_capture_positionally() {
            let result = compiled("hit {} for {}");
            let captures = result.regex.unwrap();
            let captures = captures.captures("hit orc for 12").expect("fires");
            assert_eq!(&captures[1], "orc");
            assert_eq!(&captures[2], "12");
        }

        #[test]
        fn absent_optional_does_not_participate() {
            let result = compiled("greet {person?}");
            let regex = result.regex.unwrap();
            let captures = regex.captures("greet").expect("fires");
            assert!(
                captures.name("person").is_none(),
                "an absent optional is absent, not empty"
            );
        }
    }

    mod esc_translation {
        use super::*;

        #[test]
        fn translates_esc_escape_aware() {
            // fixtures.md §6.
            assert_eq!(
                translate_esc(r"\e\[1;31m(?<hp>\d+)hp"),
                r"\x1b\[1;31m(?<hp>\d+)hp"
            );
            // `\\e` is a literal backslash then `e`, not ESC.
            assert_eq!(translate_esc(r"\\e"), r"\\e");
            // `\x1b` written directly passes through.
            assert_eq!(translate_esc(r"\x1b\[31m"), r"\x1b\[31m");
            // A trailing lone backslash survives.
            assert_eq!(translate_esc("a\\"), "a\\");
        }

        #[test]
        fn translated_raw_sources_compile_and_match() {
            let regex = Regex::new(&translate_esc(r"\e\[1;31m(?<hp>\d+)hp")).unwrap();
            let captures = regex
                .captures("\x1b[1;31m42hp")
                .expect("matches the raw line");
            assert_eq!(&captures["hp"], "42");
            assert!(!regex.is_match("42hp"));
        }
    }

    mod command_prefilters {
        use super::*;

        /// fixtures.md §2 — metacharacter names match as whole
        /// whitespace-delimited words, case-sensitively.
        #[test]
        fn whole_word_semantics() {
            let greet = Regex::new(&command_prefilter("greet")).unwrap();
            assert!(greet.is_match("greet Mira"));
            assert!(greet.is_match("greet"));
            assert!(!greet.is_match("greetings"));
            assert!(!greet.is_match("greet-me now"));
            assert!(!greet.is_match("GREET Mira"), "case-sensitive (D12)");

            let star = Regex::new(&command_prefilter("*")).unwrap();
            assert!(star.is_match("* waves hello"));
            assert!(star.is_match("*"));
            assert!(!star.is_match("*waves"));

            let hash = Regex::new(&command_prefilter("#")).unwrap();
            assert!(hash.is_match("#"));
            assert!(!hash.is_match("#3"));
        }
    }

    mod derivation {
        use super::*;

        #[test]
        fn alias_pattern_round_trips_deterministically() {
            let pattern = AliasMatcherSource::Pattern {
                source: "greet {person}".to_string(),
                anchor_start: true,
                anchor_end: true,
            };
            let stored = alias_pattern(&pattern).unwrap();
            assert_eq!(stored, r"^greet\s+(?<person>.*)$");
            // save(compile(source)) == stored: recompiling yields the same
            // string, so an untouched matcher never perturbs the file.
            assert_eq!(alias_pattern(&pattern).unwrap(), stored);

            let command = AliasMatcherSource::Command {
                name: "obe".to_string(),
                args: vec![ArgSpec {
                    name: "target".to_string(),
                    kind: ArgKind::Required,
                }],
                parse: ParseMode::All,
                mode: CmdMode::Advanced,
            };
            assert_eq!(alias_pattern(&command).unwrap(), r"^obe(?:\s|$)");
        }

        #[test]
        fn alias_pattern_reports_compile_errors() {
            let bad = AliasMatcherSource::Pattern {
                source: "hit {1}".to_string(),
                anchor_start: true,
                anchor_end: true,
            };
            assert!(matches!(
                alias_pattern(&bad).unwrap_err().as_slice(),
                [PatternError::NumberedHole { .. }]
            ));
        }

        #[test]
        fn trigger_rows_land_in_their_role_vectors() {
            let rows = vec![
                TriggerMatcherSource {
                    role: MatcherRole::Match,
                    syntax: MatcherSyntax::Pattern,
                    source: "You are {state}.".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                },
                TriggerMatcherSource {
                    role: MatcherRole::Anti,
                    syntax: MatcherSyntax::Regex,
                    source: "no longer".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                },
                TriggerMatcherSource {
                    role: MatcherRole::Raw,
                    syntax: MatcherSyntax::Regex,
                    source: r"\e\[31m".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                },
            ];
            let derived = trigger_patterns(&rows).unwrap();
            assert_eq!(derived.patterns, vec![r"^You\s+are\s+(?<state>.*?)\.$"]);
            assert_eq!(derived.anti_patterns, vec!["no longer"]);
            // Raw sources store the translated form; the sidecar keeps `\e`.
            assert_eq!(derived.raw_patterns, vec![r"\x1b\[31m"]);
        }

        #[test]
        fn trigger_rows_report_errors_by_index() {
            let rows = vec![
                TriggerMatcherSource {
                    role: MatcherRole::Match,
                    syntax: MatcherSyntax::Pattern,
                    source: "fine".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                },
                TriggerMatcherSource {
                    role: MatcherRole::Match,
                    syntax: MatcherSyntax::Regex,
                    source: "[unclosed".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                },
            ];
            let errors = trigger_patterns(&rows).unwrap_err();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].0, 1);
            assert!(matches!(errors[0].1, PatternError::Engine { .. }));
        }
    }

    mod serde_round_trips {
        use super::*;

        #[test]
        fn pattern_sidecar_is_sparse() {
            let matcher = AliasMatcherSource::Pattern {
                source: "greet {person}".to_string(),
                anchor_start: true,
                anchor_end: false,
            };
            let json = serde_json::to_string(&matcher).unwrap();
            assert_eq!(
                json,
                r#"{"kind":"pattern","source":"greet {person}","anchor_end":false}"#
            );
            assert_eq!(
                serde_json::from_str::<AliasMatcherSource>(&json).unwrap(),
                matcher
            );
        }

        #[test]
        fn command_sidecar_round_trips() {
            let matcher = AliasMatcherSource::Command {
                name: "obe".to_string(),
                args: vec![
                    ArgSpec {
                        name: "target".to_string(),
                        kind: ArgKind::Required,
                    },
                    ArgSpec {
                        name: "words".to_string(),
                        kind: ArgKind::Rest,
                    },
                ],
                parse: ParseMode::All,
                mode: CmdMode::Advanced,
            };
            let json = serde_json::to_string(&matcher).unwrap();
            assert_eq!(
                serde_json::from_str::<AliasMatcherSource>(&json).unwrap(),
                matcher
            );
        }

        #[test]
        fn minimal_command_json_fills_defaults() {
            let matcher: AliasMatcherSource =
                serde_json::from_str(r#"{"kind":"command","name":"greet"}"#).unwrap();
            assert_eq!(
                matcher,
                AliasMatcherSource::Command {
                    name: "greet".to_string(),
                    args: Vec::new(),
                    parse: ParseMode::All,
                    mode: CmdMode::Simple,
                }
            );
        }

        #[test]
        fn trigger_matcher_defaults_and_round_trip() {
            let row: TriggerMatcherSource =
                serde_json::from_str(r#"{"role":"anti","syntax":"regex","source":"no longer"}"#)
                    .unwrap();
            assert!(row.anchor_start && row.anchor_end);
            assert_eq!(row.role, MatcherRole::Anti);
            let json = serde_json::to_string(&row).unwrap();
            assert_eq!(
                serde_json::from_str::<TriggerMatcherSource>(&json).unwrap(),
                row
            );
        }
    }
}
