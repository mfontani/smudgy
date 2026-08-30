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

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
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

/// The terminal color channel that a trigger color filter checks.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MatcherColorChannel {
    #[default]
    Foreground,
    Background,
}

/// A terminal color that a trigger can match.
///
/// ANSI stores the 16-color palette slot, not the current theme RGB value. A
/// theme change does not change the filter. Xterm stores its palette index.
/// Matching preserves xterm indices 0 through 15 as ANSI colors. The VT parser
/// converts xterm indices 16 through 255 and truecolor values to RGB.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MatcherColor {
    Ansi {
        index: u8,
    },
    Xterm {
        index: u8,
    },
    Truecolor {
        r: u8,
        g: u8,
        b: u8,
        /// An optional HSV range. The RGB triplet remains the exact-match value
        /// and the fallback for readers that do not support the `range` field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<MatcherHsvRange>,
    },
}

impl Default for MatcherColor {
    fn default() -> Self {
        Self::Ansi { index: 7 }
    }
}

/// An HSV range endpoint.
///
/// Hue is an integer in degrees. The matcher normalizes it modulo 360 before use.
/// Saturation and value use the full `u8` range (`0` through `255`). They do
/// not use a lossy integer percentage.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatcherHsv {
    pub hue: u16,
    pub saturation: u8,
    pub value: u8,
}

impl MatcherHsv {
    /// The number of integer degrees in the hue circle.
    pub const HUE_PERIOD: u16 = 360;

    /// Converts an RGB color to the persisted HSV representation.
    ///
    /// This canonical integer conversion rounds to the nearest value. The UI
    /// and runtime should use it instead of separate floating-point conversions.
    #[must_use]
    pub fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        let saturation = if max == 0 {
            0
        } else {
            let numerator = u32::from(delta) * 255 + u32::from(max) / 2;
            u8::try_from(numerator / u32::from(max)).unwrap_or(255)
        };
        let hue = if delta == 0 {
            0
        } else {
            let (offset, difference) = if max == r {
                (0, i32::from(g) - i32::from(b))
            } else if max == g {
                (120, i32::from(b) - i32::from(r))
            } else {
                (240, i32::from(r) - i32::from(g))
            };
            let numerator = difference * 60;
            let denominator = i32::from(delta);
            let rounded = if numerator >= 0 {
                (numerator + denominator / 2) / denominator
            } else {
                (numerator - denominator / 2) / denominator
            };
            u16::try_from((offset + rounded).rem_euclid(i32::from(Self::HUE_PERIOD))).unwrap_or(0)
        };

        Self {
            hue,
            saturation,
            value: max,
        }
    }

    /// Converts a persisted HSV endpoint to its canonical RGB preview color.
    #[must_use]
    pub fn to_rgb(self) -> (u8, u8, u8) {
        let hsv = self.normalized();
        if hsv.saturation == 0 {
            return (hsv.value, hsv.value, hsv.value);
        }

        let chroma = (u32::from(hsv.value) * u32::from(hsv.saturation) + 127) / 255;
        let sector = hsv.hue / 60;
        let remainder = u32::from(hsv.hue % 60);
        let secondary_numerator = if sector.is_multiple_of(2) {
            chroma * remainder
        } else {
            chroma * (60 - remainder)
        };
        let secondary = (secondary_numerator + 30) / 60;
        let minimum = u32::from(hsv.value) - chroma;
        let (r, g, b) = match sector {
            0 => (chroma, secondary, 0),
            1 => (secondary, chroma, 0),
            2 => (0, chroma, secondary),
            3 => (0, secondary, chroma),
            4 => (secondary, 0, chroma),
            _ => (chroma, 0, secondary),
        };
        let component = |value: u32| u8::try_from(value + minimum).unwrap_or(255);
        (component(r), component(g), component(b))
    }

    /// Converts this endpoint to 8-bit RGB and back to canonical HSV.
    ///
    /// An achromatic RGB swatch cannot show its hue. This method preserves the
    /// specified hue because it remains useful as a range boundary. Matchers do
    /// not compare hue for an achromatic input color.
    #[must_use]
    pub fn rgb_canonicalized(self) -> Self {
        let authored = self.normalized();
        let (r, g, b) = authored.to_rgb();
        let mut canonical = Self::from_rgb(r, g, b);
        if canonical.saturation == 0 {
            canonical.hue = authored.hue;
        }
        canonical
    }

    /// Returns this endpoint with its hue in the canonical `0..360` interval.
    #[must_use]
    pub const fn normalized(self) -> Self {
        Self {
            hue: self.hue % Self::HUE_PERIOD,
            saturation: self.saturation,
            value: self.value,
        }
    }
}

/// An axis-aligned match region in HSV color space. The two endpoints define
/// opposite corners.
///
/// Saturation and value have unordered bounds. Hue is a directed interval from
/// the UI's "From" endpoint to its "To" endpoint, moving through increasing
/// degrees. For example, 350° to 10° crosses 0° and selects a narrow range.
/// The persisted `first`, `second`, and `wrap_hue` fields retain the original
/// storage format. Use [`Self::from_to`] and [`Self::directed_endpoints`] instead
/// of interpreting these fields as UI endpoint labels.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatcherHsvRange {
    pub first: MatcherHsv,
    pub second: MatcherHsv,
    /// The legacy storage flag for a hue interval that crosses 0°.
    ///
    /// Keep this field for saved-file compatibility. New code must derive it
    /// with [`Self::from_to`]. [`Self::directed_endpoints`] also converts saved
    /// ranges whose field order does not agree with this flag.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wrap_hue: bool,
}

impl MatcherHsvRange {
    /// Creates a range directed from `from` to `to` through increasing hue.
    ///
    /// The method normalizes both stored hues to `0..360`. When `to` has a
    /// smaller hue, the interval continues through 360° and then 0°. Equal
    /// hues select one hue. Saturation and value remain unordered bounds.
    #[must_use]
    pub const fn from_to(from: MatcherHsv, to: MatcherHsv) -> Self {
        let from = from.normalized();
        let to = to.normalized();
        Self {
            first: from,
            second: to,
            wrap_hue: from.hue > to.hue,
        }
    }

    /// Returns this saved range as directed `(from, to)` UI endpoints.
    ///
    /// Older editors treated `first` and `second` as unordered. This method
    /// uses the saved `wrap_hue` flag to orient those endpoints without
    /// changing the matched hue, saturation, or value intervals. Returned hues
    /// are in `0..360`. Rebuilding the result with [`Self::from_to`] preserves
    /// the saved match semantics. An old wrapped range with equal hues becomes
    /// an unwrapped equal-hue range. Both forms select only that hue.
    #[must_use]
    pub const fn directed_endpoints(self) -> (MatcherHsv, MatcherHsv) {
        let first = self.first.normalized();
        let second = self.second.normalized();
        let order_matches_direction = first.hue == second.hue
            || (self.wrap_hue && first.hue > second.hue)
            || (!self.wrap_hue && first.hue < second.hue);
        if order_matches_direction {
            (first, second)
        } else {
            (second, first)
        }
    }

    /// Converts both endpoints to the HSV values that their 8-bit RGB swatches
    /// represent.
    ///
    /// The runtime represents truecolor input as RGB. This conversion makes
    /// narrow ranges match the colors that the editor shows. It establishes
    /// the saved direction before quantization because quantization can move
    /// one endpoint across 0°.
    #[must_use]
    pub fn rgb_canonicalized(self) -> Self {
        let (from, to) = self.directed_endpoints();
        Self::from_to(from.rgb_canonicalized(), to.rgb_canonicalized())
    }

    /// Returns the inclusive directed hue interval as increasing degrees.
    ///
    /// The first value is in `0..360`. The second value can be in `360..720`
    /// when the interval crosses 0°. For example, 350° to 10° returns
    /// `(350, 370)`. This 360..720 representation lets callers test one linear
    /// interval.
    #[must_use]
    pub const fn directed_hue_bounds(self) -> (u16, u16) {
        let (from, to) = self.directed_endpoints();
        let lifted_to = if to.hue < from.hue {
            to.hue + MatcherHsv::HUE_PERIOD
        } else {
            to.hue
        };
        (from.hue, lifted_to)
    }

    /// Returns inclusive, sorted hue bounds in integer degrees.
    ///
    /// This method supports code that also reads the legacy `wrap_hue` flag.
    /// Use [`Self::directed_hue_bounds`] for new directed-range code.
    #[must_use]
    pub const fn hue_bounds(self) -> (u16, u16) {
        let first = self.first.hue % MatcherHsv::HUE_PERIOD;
        let second = self.second.hue % MatcherHsv::HUE_PERIOD;
        if first <= second {
            (first, second)
        } else {
            (second, first)
        }
    }

    /// Returns true if `hue` is in the selected circular interval.
    #[must_use]
    pub const fn hue_matches(self, hue: u16) -> bool {
        let hue = hue % MatcherHsv::HUE_PERIOD;
        let (minimum, maximum) = self.hue_bounds();
        if minimum == maximum {
            hue == minimum
        } else if self.wrap_hue {
            hue <= minimum || hue >= maximum
        } else {
            hue >= minimum && hue <= maximum
        }
    }

    /// Returns inclusive saturation bounds on the `0..=255` scale.
    #[must_use]
    pub const fn saturation_bounds(self) -> (u8, u8) {
        if self.first.saturation <= self.second.saturation {
            (self.first.saturation, self.second.saturation)
        } else {
            (self.second.saturation, self.first.saturation)
        }
    }

    /// Returns inclusive value bounds on the `0..=255` scale.
    #[must_use]
    pub const fn value_bounds(self) -> (u8, u8) {
        if self.first.value <= self.second.value {
            (self.first.value, self.second.value)
        } else {
            (self.second.value, self.first.value)
        }
    }
}

/// An SGR text attribute that a color filter can require.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatcherTextAttribute {
    Bold,
    Faint,
    Italic,
    Underline,
    DoubleUnderline,
    SlowBlink,
    FastBlink,
    CrossedOut,
    Reverse,
}

/// A color filter for a normal pattern or anti-pattern.
///
/// `None` accepts any color in that channel. The filter can constrain the
/// foreground, background, or both. An empty attribute list accepts any
/// attributes. Otherwise, the filter requires every listed attribute.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct MatcherColorMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<MatcherColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<MatcherColor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<MatcherTextAttribute>,
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
        /// The command word, when the author pinned one that differs from the
        /// alias's own name. `None` — the ordinary case — means the alias name
        /// *is* the command, so renaming the alias renames the command.
        ///
        /// The override exists for the words a name cannot spell: `*` and `?`
        /// are commands players actually type but are illegal filename
        /// characters (see [`crate::models::naming`]), and a name may contain
        /// spaces while a command word is a single token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
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
    /// An optional color filter. Raw matchers ignore this field. They run their
    /// escape-aware regex against raw input, including terminal escape bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<MatcherColorMatch>,
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

/// The runtime half of a Command alias: what the matcher needs at match time
/// to decide firing and produce captures. Built from the alias sidecar when a
/// definition loads — the one place the runtime reads authoring state. Safe
/// against a stale or lying sidecar: [`assign`] re-checks the first word
/// itself, so a mismatch between the stored prefilter and this spec can only
/// fall through, never fire wrongly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: String,
    pub args: Vec<ArgSpec>,
    pub parse: ParseMode,
}

impl AliasMatcherSource {
    /// The word this sidecar matches on: the pinned override, or — the
    /// ordinary case — the alias's own name. `None` for a Pattern.
    #[must_use]
    pub fn command_word<'a>(&'a self, alias_name: &'a str) -> Option<&'a str> {
        match self {
            Self::Command { name, .. } => Some(name.as_deref().unwrap_or(alias_name)),
            Self::Pattern { .. } => None,
        }
    }

    /// The runtime [`CommandSpec`] this sidecar implies; `None` for a Pattern.
    /// The spec's `name` is the *resolved* command word, so the runtime never
    /// has to know an override was involved.
    #[must_use]
    pub fn command_spec(&self, alias_name: &str) -> Option<CommandSpec> {
        match self {
            Self::Command {
                name, args, parse, ..
            } => Some(CommandSpec {
                name: name.as_deref().unwrap_or(alias_name).to_string(),
                args: args.clone(),
                parse: *parse,
            }),
            Self::Pattern { .. } => None,
        }
    }
}

/// One Command token: its grouped/unescaped value and the byte offset of its
/// first character in the tokenized string (which a `Rest` argument uses to
/// slice the raw remainder, spacing and quoting intact).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub value: String,
    pub start: usize,
}

/// A Command tokenizer failure. Distinct from a mere non-match: the editor
/// surfaces these verbatim in the Try-it verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenizeError {
    UnterminatedQuote,
    UnbalancedBraces,
}

/// Why a Command alias did not fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandMiss {
    /// No tokens at all.
    Empty,
    /// The first whitespace-delimited word is not the command name
    /// (case-sensitive, D12).
    WrongFirstWord,
    /// A required argument had no token — the D10 outcome: the runtime echoes
    /// the usage line locally and swallows the input.
    MissingRequired {
        name: String,
    },
    /// Tokens remain after every argument was satisfied and there is no
    /// `Rest` argument to claim them; the typed line falls through.
    Unclaimed {
        text: String,
    },
    Tokenize(TokenizeError),
}

/// The result of matching one typed line against a Command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// One entry per [`ArgSpec`], in declaration order. `None` is an absent
    /// optional — distinct from an empty string, though the runtime's capture
    /// list flattens both to empty (the regex convention for a group that did
    /// not participate); the editor's Try-it keeps the distinction.
    Fired {
        args: Vec<(String, Option<String>)>,
    },
    NotFired(CommandMiss),
}

/// Splits a Command's argument text into tokens per the parse mode
/// (`matching-logic.md` §1). Backslash escapes the next character in every
/// context; quotes do not nest, braces do.
///
/// # Errors
///
/// Returns [`TokenizeError`] for an unterminated quote or unbalanced braces.
#[allow(clippy::missing_panics_doc)] // the depth bookkeeping cannot underflow
pub fn tokenize(input: &str, parse: ParseMode) -> Result<Vec<Token>, TokenizeError> {
    let spaces_split = parse != ParseMode::Raw;
    let quotes_group = matches!(parse, ParseMode::Quotes | ParseMode::All);
    let braces_group = matches!(parse, ParseMode::Braces | ParseMode::All);

    if !spaces_split {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![Token {
            value: trimmed.to_string(),
            start: input.len() - input.trim_start().len(),
        }]);
    }

    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].1.is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = chars[i].0;
        let mut value = String::new();

        if braces_group && chars[i].1 == '{' {
            let mut depth = 0i32;
            loop {
                if i >= chars.len() {
                    break;
                }
                let c = chars[i].1;
                if c == '\\' && i + 1 < chars.len() {
                    value.push(chars[i + 1].1);
                    i += 2;
                    continue;
                }
                if c == '{' {
                    depth += 1;
                    if depth == 1 {
                        i += 1;
                        continue;
                    }
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                value.push(c);
                i += 1;
            }
            if depth != 0 {
                return Err(TokenizeError::UnbalancedBraces);
            }
            tokens.push(Token { value, start });
            continue;
        }

        // A bare token runs to the next whitespace; a quoted segment inside it
        // glues into the same token, shell-style (`"ugly"}` is one token,
        // `ugly}`), which is what the design fixtures pin.
        while i < chars.len() && !chars[i].1.is_whitespace() {
            let c = chars[i].1;
            if quotes_group && (c == '"' || c == '\'') {
                let quote = c;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    let c = chars[i].1;
                    if c == '\\' && i + 1 < chars.len() {
                        value.push(chars[i + 1].1);
                        i += 2;
                        continue;
                    }
                    if c == quote {
                        i += 1;
                        closed = true;
                        break;
                    }
                    value.push(c);
                    i += 1;
                }
                if !closed {
                    return Err(TokenizeError::UnterminatedQuote);
                }
                continue;
            }
            if c == '\\' && i + 1 < chars.len() {
                value.push(chars[i + 1].1);
                i += 2;
                continue;
            }
            value.push(c);
            i += 1;
        }
        tokens.push(Token { value, start });
    }
    Ok(tokens)
}

/// Matches one typed line against a Command: tokenize, check the command
/// word, then assign tokens to arguments (`matching-logic.md` §2). Required
/// args consume first; an optional yields only when enough tokens remain for
/// the required args after it; a `Rest` arg takes the **raw** remainder of
/// the trimmed input, spacing and quoting preserved.
#[must_use]
pub fn assign(input: &str, name: &str, specs: &[ArgSpec], parse: ParseMode) -> CommandOutcome {
    let trimmed = input.trim();
    let tokens = match tokenize(trimmed, parse) {
        Ok(tokens) => tokens,
        Err(err) => return CommandOutcome::NotFired(CommandMiss::Tokenize(err)),
    };
    let Some(first) = tokens.first() else {
        return CommandOutcome::NotFired(CommandMiss::Empty);
    };
    if first.value != name {
        return CommandOutcome::NotFired(CommandMiss::WrongFirstWord);
    }
    let rest = &tokens[1..];

    let required_after = |n: usize| {
        specs[n + 1..]
            .iter()
            .filter(|s| s.kind == ArgKind::Required)
            .count()
    };

    let mut args: Vec<(String, Option<String>)> = Vec::with_capacity(specs.len());
    let mut idx = 0usize;
    for (n, spec) in specs.iter().enumerate() {
        match spec.kind {
            ArgKind::Required => {
                if idx >= rest.len() {
                    return CommandOutcome::NotFired(CommandMiss::MissingRequired {
                        name: spec.name.clone(),
                    });
                }
                args.push((spec.name.clone(), Some(rest[idx].value.clone())));
                idx += 1;
            }
            ArgKind::Optional => {
                if rest.len() - idx > required_after(n) {
                    args.push((spec.name.clone(), Some(rest[idx].value.clone())));
                    idx += 1;
                } else {
                    args.push((spec.name.clone(), None));
                }
            }
            ArgKind::Rest => {
                if idx < rest.len() {
                    args.push((
                        spec.name.clone(),
                        Some(trimmed[rest[idx].start..].to_string()),
                    ));
                    idx = rest.len();
                } else {
                    args.push((spec.name.clone(), None));
                }
                // Rest is always last (the editor enforces it).
                break;
            }
        }
    }

    if idx < rest.len() {
        let text = rest[idx..]
            .iter()
            .map(|t| t.value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        return CommandOutcome::NotFired(CommandMiss::Unclaimed { text });
    }
    CommandOutcome::Fired { args }
}

/// The generated usage line — `greet <person> [words...]` — used by both the
/// editor's `Usage` row and the D10 missing-argument echo, so the two cannot
/// drift.
#[must_use]
pub fn usage_line(name: &str, args: &[ArgSpec]) -> String {
    let mut out = String::from(name);
    for arg in args {
        out.push(' ');
        match arg.kind {
            ArgKind::Required => {
                out.push('<');
                out.push_str(&arg.name);
                out.push('>');
            }
            ArgKind::Optional => {
                out.push('[');
                out.push_str(&arg.name);
                out.push(']');
            }
            ArgKind::Rest => {
                out.push('[');
                out.push_str(&arg.name);
                out.push_str("...]");
            }
        }
    }
    out
}

/// The stored `pattern` derived from an alias sidecar — what a save writes.
/// `alias_name` supplies the command word for a Command with no override, so
/// a rename recompiles the prefilter.
///
/// # Errors
///
/// Returns the pattern's compile errors; a Command prefilter cannot fail.
pub fn alias_pattern(
    matcher: &AliasMatcherSource,
    alias_name: &str,
) -> Result<String, Vec<PatternError>> {
    match matcher {
        AliasMatcherSource::Command { name, .. } => {
            Ok(command_prefilter(name.as_deref().unwrap_or(alias_name)))
        }
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
        // A blank row with a color filter is a color-only matcher. The compiler
        // stores an empty regex for both editor syntaxes. The runtime recognizes
        // this exact form and scans styled spans directly.
        // `Trigger::new` keeps blank unfiltered rows inactive for compatibility.
        if matcher.source.is_empty() && matcher.color.is_some() && matcher.role != MatcherRole::Raw
        {
            match matcher.role {
                MatcherRole::Match => derived.patterns.push(String::new()),
                MatcherRole::Anti => derived.anti_patterns.push(String::new()),
                MatcherRole::Raw => unreachable!(),
            }
            continue;
        }
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

    mod tokenizer {
        use super::*;

        fn values(input: &str, parse: ParseMode) -> Vec<String> {
            tokenize(input, parse)
                .expect("tokenizes")
                .into_iter()
                .map(|t| t.value)
                .collect()
        }

        /// fixtures.md §1 — tokens by parse mode.
        #[test]
        fn fixture_rows() {
            use ParseMode::{All, Braces, Quotes, Raw, Spaces};
            assert_eq!(
                values(r#"big "ugly" troll"#, Spaces),
                ["big", "\"ugly\"", "troll"]
            );
            assert_eq!(values("big   ugly", Spaces), ["big", "ugly"]);
            assert_eq!(values(r#""big ugly" troll"#, Quotes), ["big ugly", "troll"]);
            assert_eq!(values("'big ugly' troll", Quotes), ["big ugly", "troll"]);
            assert_eq!(
                tokenize(r#""big ugly troll"#, Quotes),
                Err(TokenizeError::UnterminatedQuote)
            );
            assert_eq!(values(r#"say \"hi\""#, Quotes), ["say", "\"hi\""]);
            // A closed quote glues into the surrounding bare run.
            assert_eq!(
                values(r#"{big "ugly"} troll"#, Quotes),
                ["{big", "ugly}", "troll"]
            );
            assert_eq!(
                values(r#"{big "ugly"} troll"#, Braces),
                ["big \"ugly\"", "troll"]
            );
            assert_eq!(
                values("{cast {magic missile}} now", Braces),
                ["cast {magic missile}", "now"]
            );
            assert_eq!(
                tokenize("{big ugly", Braces),
                Err(TokenizeError::UnbalancedBraces)
            );
            assert_eq!(
                values(r#""big ugly" troll"#, Braces),
                ["\"big", "ugly\"", "troll"]
            );
            assert_eq!(
                values(r#""big ugly" {a "gift"}"#, All),
                ["big ugly", "a \"gift\""]
            );
            assert_eq!(values("big ugly troll", Raw), ["big ugly troll"]);
            assert_eq!(values("  big  ugly  ", Raw), ["big  ugly"]);
        }
    }

    mod command_assignment {
        use super::*;

        fn spec(name: &str, kind: ArgKind) -> ArgSpec {
            ArgSpec {
                name: name.to_string(),
                kind,
            }
        }

        fn fired(outcome: &CommandOutcome) -> &[(String, Option<String>)] {
            match outcome {
                CommandOutcome::Fired { args } => args,
                CommandOutcome::NotFired(miss) => panic!("expected a fire, got {miss:?}"),
            }
        }

        fn arg(outcome: &CommandOutcome, name: &str) -> Option<String> {
            fired(outcome)
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, v)| v.clone())
        }

        /// fixtures.md §2 — command matching, parse mode All unless noted.
        #[test]
        fn fixture_rows() {
            use ArgKind::{Optional, Required, Rest};
            let all = ParseMode::All;

            // No args.
            assert!(matches!(
                assign("greet", "greet", &[], all),
                CommandOutcome::Fired { .. }
            ));
            assert!(matches!(
                assign("GREET", "greet", &[], all),
                CommandOutcome::NotFired(CommandMiss::WrongFirstWord)
            ));
            for input in ["lobe", "obey", "greeting"] {
                assert!(
                    matches!(
                        assign(input, "greet", &[], all),
                        CommandOutcome::NotFired(CommandMiss::WrongFirstWord)
                    ),
                    "{input}"
                );
            }
            assert!(matches!(
                assign("greet Mira", "greet", &[], all),
                CommandOutcome::NotFired(CommandMiss::Unclaimed { .. })
            ));

            let required = [spec("person", Required)];
            let o = assign("greet Mira", "greet", &required, all);
            assert_eq!(arg(&o, "person").as_deref(), Some("Mira"));
            assert!(matches!(
                assign("greet", "greet", &required, all),
                CommandOutcome::NotFired(CommandMiss::MissingRequired { .. })
            ));
            let o = assign(r#"greet "big ugly troll""#, "greet", &required, all);
            assert_eq!(arg(&o, "person").as_deref(), Some("big ugly troll"));

            let optional = [spec("person", Optional)];
            let o = assign("greet", "greet", &optional, all);
            assert_eq!(arg(&o, "person"), None, "absent, not empty-string");
            let o = assign("greet Mira", "greet", &optional, all);
            assert_eq!(arg(&o, "person").as_deref(), Some("Mira"));

            let rest = [spec("person", Rest)];
            let o = assign("greet Mira and Bob", "greet", &rest, all);
            assert_eq!(arg(&o, "person").as_deref(), Some("Mira and Bob"));
            // The raw remainder: spacing and quoting preserved.
            let o = assign(r#"greet "Mira"  and   Bob"#, "greet", &rest, all);
            assert_eq!(arg(&o, "person").as_deref(), Some("\"Mira\"  and   Bob"));

            let req_rest = [spec("person", Required), spec("mood", Rest)];
            let o = assign("greet Mira warmly and often", "greet", &req_rest, all);
            assert_eq!(arg(&o, "person").as_deref(), Some("Mira"));
            assert_eq!(arg(&o, "mood").as_deref(), Some("warmly and often"));

            // An optional yields to a later required.
            let opt_req = [spec("person", Optional), spec("target", Required)];
            let o = assign("greet Bob", "greet", &opt_req, all);
            assert_eq!(arg(&o, "person"), None);
            assert_eq!(arg(&o, "target").as_deref(), Some("Bob"));
            let o = assign("greet Mira Bob", "greet", &opt_req, all);
            assert_eq!(arg(&o, "person").as_deref(), Some("Mira"));
            assert_eq!(arg(&o, "target").as_deref(), Some("Bob"));

            assert!(matches!(
                assign("greet Mira Bob", "greet", &required, all),
                CommandOutcome::NotFired(CommandMiss::Unclaimed { .. })
            ));
            assert!(matches!(
                assign(
                    r#"greet "big ugly troll""#,
                    "greet",
                    &required,
                    ParseMode::Spaces
                ),
                CommandOutcome::NotFired(CommandMiss::Unclaimed { .. })
            ));

            // Metacharacter names match as whole words, case-sensitively.
            let star_rest = [spec("person", Rest)];
            let o = assign("* waves hello", "*", &star_rest, all);
            assert_eq!(arg(&o, "person").as_deref(), Some("waves hello"));
            assert!(matches!(
                assign("*waves", "*", &star_rest, all),
                CommandOutcome::NotFired(CommandMiss::WrongFirstWord)
            ));
        }

        #[test]
        fn usage_line_matches_the_deck_format() {
            let args = [
                spec("target", ArgKind::Required),
                spec("count", ArgKind::Optional),
                spec("words", ArgKind::Rest),
            ];
            assert_eq!(usage_line("obe", &args), "obe <target> [count] [words...]");
            assert_eq!(usage_line("greet", &[]), "greet");
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
            let stored = alias_pattern(&pattern, "greet").unwrap();
            assert_eq!(stored, r"^greet\s+(?<person>.*)$");
            // save(compile(source)) == stored: recompiling yields the same
            // string, so an untouched matcher never perturbs the file.
            assert_eq!(alias_pattern(&pattern, "greet").unwrap(), stored);

            let args = vec![ArgSpec {
                name: "target".to_string(),
                kind: ArgKind::Required,
            }];
            // No override: the alias name is the command, so a rename
            // recompiles the prefilter.
            let inherited = AliasMatcherSource::Command {
                name: None,
                args: args.clone(),
                parse: ParseMode::All,
                mode: CmdMode::Advanced,
            };
            assert_eq!(alias_pattern(&inherited, "obe").unwrap(), r"^obe(?:\s|$)");
            assert_eq!(alias_pattern(&inherited, "bash").unwrap(), r"^bash(?:\s|$)");

            // An override pins the word a name cannot spell, and the name
            // stops feeding the prefilter.
            let pinned = AliasMatcherSource::Command {
                name: Some("*".to_string()),
                args,
                parse: ParseMode::All,
                mode: CmdMode::Advanced,
            };
            assert_eq!(
                alias_pattern(&pinned, "star-emote").unwrap(),
                r"^\*(?:\s|$)"
            );
        }

        #[test]
        fn alias_pattern_reports_compile_errors() {
            let bad = AliasMatcherSource::Pattern {
                source: "hit {1}".to_string(),
                anchor_start: true,
                anchor_end: true,
            };
            assert!(matches!(
                alias_pattern(&bad, "hit").unwrap_err().as_slice(),
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
                    color: None,
                },
                TriggerMatcherSource {
                    role: MatcherRole::Anti,
                    syntax: MatcherSyntax::Regex,
                    source: "no longer".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                    color: None,
                },
                TriggerMatcherSource {
                    role: MatcherRole::Raw,
                    syntax: MatcherSyntax::Regex,
                    source: r"\e\[31m".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                    color: None,
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
                    color: None,
                },
                TriggerMatcherSource {
                    role: MatcherRole::Match,
                    syntax: MatcherSyntax::Regex,
                    source: "[unclosed".to_string(),
                    anchor_start: true,
                    anchor_end: true,
                    color: None,
                },
            ];
            let errors = trigger_patterns(&rows).unwrap_err();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].0, 1);
            assert!(matches!(errors[0].1, PatternError::Engine { .. }));
        }

        #[test]
        fn blank_color_rows_compile_as_color_only_matchers() {
            let rows = vec![TriggerMatcherSource {
                role: MatcherRole::Match,
                syntax: MatcherSyntax::Pattern,
                source: String::new(),
                anchor_start: true,
                anchor_end: true,
                color: Some(MatcherColorMatch {
                    foreground: Some(MatcherColor::Ansi { index: 1 }),
                    background: None,
                    attributes: Vec::new(),
                }),
            }];
            assert_eq!(trigger_patterns(&rows).unwrap().patterns, vec![""]);
        }

        #[test]
        fn whitespace_colored_regex_remains_a_text_matcher() {
            let rows = vec![TriggerMatcherSource {
                role: MatcherRole::Match,
                syntax: MatcherSyntax::Regex,
                source: " ".to_string(),
                anchor_start: true,
                anchor_end: true,
                color: Some(MatcherColorMatch {
                    foreground: Some(MatcherColor::Ansi { index: 1 }),
                    background: None,
                    attributes: Vec::new(),
                }),
            }];
            assert_eq!(trigger_patterns(&rows).unwrap().patterns, vec![" "]);
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
                name: Some("obe".to_string()),
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
                serde_json::from_str(r#"{"kind":"command"}"#).unwrap();
            assert_eq!(
                matcher,
                AliasMatcherSource::Command {
                    name: None,
                    args: Vec::new(),
                    parse: ParseMode::All,
                    mode: CmdMode::Simple,
                }
            );
            // An absent override inherits the alias name; the field only
            // reappears on disk once one is pinned.
            assert_eq!(matcher.command_word("greet"), Some("greet"));
            assert_eq!(
                serde_json::to_string(&matcher).unwrap(),
                r#"{"kind":"command","parse":"all","mode":"simple"}"#
            );

            let pinned: AliasMatcherSource =
                serde_json::from_str(r#"{"kind":"command","name":"*"}"#).unwrap();
            assert_eq!(pinned.command_word("star-emote"), Some("*"));
            assert!(
                serde_json::to_string(&pinned)
                    .unwrap()
                    .contains(r#""name":"*""#)
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

        #[test]
        fn trigger_color_filter_round_trips_both_channels_and_attributes() {
            let filter = MatcherColorMatch {
                foreground: Some(MatcherColor::Ansi { index: 9 }),
                background: Some(MatcherColor::Truecolor {
                    r: 1,
                    g: 2,
                    b: 3,
                    range: None,
                }),
                attributes: vec![MatcherTextAttribute::Bold, MatcherTextAttribute::Italic],
            };
            let json = serde_json::to_string(&filter).unwrap();
            assert_eq!(
                serde_json::from_str::<MatcherColorMatch>(&json).unwrap(),
                filter
            );
            assert!(json.contains("foreground"));
            assert!(json.contains("background"));
            assert!(json.contains("bold"));
        }

        #[test]
        fn exact_truecolor_json_stays_sparse_and_backward_compatible() {
            let color = MatcherColor::Truecolor {
                r: 1,
                g: 2,
                b: 3,
                range: None,
            };
            let json = serde_json::to_string(&color).unwrap();
            assert_eq!(json, r#"{"kind":"truecolor","r":1,"g":2,"b":3}"#);
            assert_eq!(serde_json::from_str::<MatcherColor>(&json).unwrap(), color);
        }

        #[test]
        fn truecolor_hsv_range_round_trips_and_normalizes_bounds() {
            let range = MatcherHsvRange {
                first: MatcherHsv {
                    hue: 721,
                    saturation: 240,
                    value: 30,
                },
                second: MatcherHsv {
                    hue: 330,
                    saturation: 40,
                    value: 210,
                },
                wrap_hue: false,
            };
            assert_eq!(range.first.normalized().hue, 1);
            assert_eq!(range.hue_bounds(), (1, 330));
            assert_eq!(range.saturation_bounds(), (40, 240));
            assert_eq!(range.value_bounds(), (30, 210));

            let color = MatcherColor::Truecolor {
                r: 128,
                g: 64,
                b: 32,
                range: Some(range),
            };
            let json = serde_json::to_string(&color).unwrap();
            assert!(json.contains(r#""range""#));
            assert_eq!(serde_json::from_str::<MatcherColor>(&json).unwrap(), color);
        }

        #[test]
        fn wrapped_hue_range_crosses_zero_and_serializes_explicitly() {
            let range = MatcherHsvRange {
                first: MatcherHsv {
                    hue: 350,
                    saturation: 0,
                    value: 0,
                },
                second: MatcherHsv {
                    hue: 10,
                    saturation: 255,
                    value: 255,
                },
                wrap_hue: true,
            };
            assert!(range.hue_matches(355));
            assert!(range.hue_matches(5));
            assert!(!range.hue_matches(180));
            assert!(serde_json::to_string(&range).unwrap().contains("wrap_hue"));
        }

        #[test]
        fn from_to_hue_range_follows_increasing_degrees() {
            let endpoint = |hue| MatcherHsv {
                hue,
                saturation: 255,
                value: 255,
            };

            let narrow = MatcherHsvRange::from_to(endpoint(350), endpoint(10));
            assert_eq!((narrow.first.hue, narrow.second.hue), (350, 10));
            assert_eq!(narrow.directed_hue_bounds(), (350, 370));
            assert!(narrow.wrap_hue);
            assert!(narrow.hue_matches(355));
            assert!(narrow.hue_matches(5));
            assert!(!narrow.hue_matches(180));

            let broad = MatcherHsvRange::from_to(endpoint(10), endpoint(350));
            assert_eq!((broad.first.hue, broad.second.hue), (10, 350));
            assert_eq!(broad.directed_hue_bounds(), (10, 350));
            assert!(!broad.wrap_hue);
            assert!(broad.hue_matches(180));
            assert!(!broad.hue_matches(5));
            assert!(!broad.hue_matches(355));

            let equal = MatcherHsvRange::from_to(endpoint(45), endpoint(45));
            assert!(!equal.wrap_hue);
            assert!(equal.hue_matches(45));
            assert!(!equal.hue_matches(44));

            let normalized = MatcherHsvRange::from_to(endpoint(721), endpoint(720));
            assert_eq!((normalized.first.hue, normalized.second.hue), (1, 0));
            assert!(normalized.wrap_hue);
        }

        #[test]
        fn directed_endpoints_preserve_legacy_range_semantics() {
            let endpoint = |hue, saturation, value| MatcherHsv {
                hue,
                saturation,
                value,
            };
            let legacy_ranges = [
                MatcherHsvRange {
                    first: endpoint(350, 20, 220),
                    second: endpoint(10, 200, 40),
                    wrap_hue: false,
                },
                MatcherHsvRange {
                    first: endpoint(10, 200, 40),
                    second: endpoint(350, 20, 220),
                    wrap_hue: true,
                },
            ];

            for legacy in legacy_ranges {
                let (from, to) = legacy.directed_endpoints();
                let rebuilt = MatcherHsvRange::from_to(from, to);
                for hue in 0..MatcherHsv::HUE_PERIOD {
                    assert_eq!(
                        rebuilt.hue_matches(hue),
                        legacy.hue_matches(hue),
                        "legacy {legacy:?} changed at hue {hue}",
                    );
                }
                assert_eq!(rebuilt.saturation_bounds(), legacy.saturation_bounds());
                assert_eq!(rebuilt.value_bounds(), legacy.value_bounds());
            }

            assert_eq!(
                legacy_ranges[0].directed_endpoints(),
                (endpoint(10, 200, 40), endpoint(350, 20, 220)),
            );
            assert_eq!(
                legacy_ranges[1].directed_endpoints(),
                (endpoint(350, 20, 220), endpoint(10, 200, 40)),
            );
        }

        #[test]
        fn rgb_canonicalization_preserves_direction_across_zero() {
            let from = MatcherHsv {
                hue: 359,
                saturation: 10,
                value: 30,
            };
            let to = MatcherHsv {
                hue: 1,
                saturation: 255,
                value: 255,
            };
            let canonical = MatcherHsvRange::from_to(from, to).rgb_canonicalized();
            let canonical_from = from.rgb_canonicalized();
            let canonical_to = to.rgb_canonicalized();

            // RGB quantization moves only the low-saturation endpoint across
            // 0°. The resulting range must remain the narrow directed arc.
            assert_eq!(canonical_from.hue, 0);
            assert_eq!(canonical_to.hue, 1);
            assert_eq!(
                canonical.directed_endpoints(),
                (canonical_from, canonical_to)
            );
            assert_eq!(canonical.directed_hue_bounds(), (0, 1));
            assert!(canonical.hue_matches(0));
            assert!(canonical.hue_matches(1));
            assert!(!canonical.hue_matches(180));
        }

        #[test]
        fn fast_hue_matcher_equals_lifted_directed_interval() {
            const ENDPOINT_HUES: [u16; 13] =
                [0, 1, 10, 45, 179, 180, 181, 349, 350, 358, 359, 360, 721];
            let endpoint = |hue| MatcherHsv {
                hue,
                saturation: 128,
                value: 128,
            };

            for first in ENDPOINT_HUES {
                for second in ENDPOINT_HUES {
                    for wrap_hue in [false, true] {
                        let range = MatcherHsvRange {
                            first: endpoint(first),
                            second: endpoint(second),
                            wrap_hue,
                        };
                        let (from, to) = range.directed_hue_bounds();
                        for hue in 0..MatcherHsv::HUE_PERIOD {
                            let lifted_hue = if hue < from {
                                hue + MatcherHsv::HUE_PERIOD
                            } else {
                                hue
                            };
                            let lifted_match = (from..=to).contains(&lifted_hue);
                            assert_eq!(
                                range.hue_matches(hue),
                                lifted_match,
                                "range {range:?} differs at hue {hue}",
                            );
                        }
                    }
                }
            }
        }

        #[test]
        fn matcher_hsv_quantization_is_exact_for_selected_colors() {
            for rgb in [
                (0, 255, 255),
                (128, 128, 128),
                (106, 60, 60),
                (64, 96, 160),
                (123, 45, 67),
            ] {
                let hsv = MatcherHsv::from_rgb(rgb.0, rgb.1, rgb.2);
                assert_eq!(hsv.to_rgb(), rgb, "{rgb:?} quantized to {hsv:?}");
            }

            assert_eq!(
                MatcherHsv::from_rgb(0, 255, 255),
                MatcherHsv {
                    hue: 180,
                    saturation: 255,
                    value: 255,
                }
            );

            let canonical = MatcherHsv {
                hue: 100,
                saturation: 100,
                value: 100,
            }
            .rgb_canonicalized();
            let (r, g, b) = canonical.to_rgb();
            assert_eq!(MatcherHsv::from_rgb(r, g, b), canonical);

            assert_eq!(
                MatcherHsv {
                    hue: 217,
                    saturation: 0,
                    value: 128,
                }
                .rgb_canonicalized()
                .hue,
                217,
            );
        }
    }
}
