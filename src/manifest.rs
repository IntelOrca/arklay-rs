//! Pack manifest (`manifest.toml`).
//!
//! The manifest is an ordinary pack entry that describes its pack: the format
//! version, a stable id, a human name and version, whether the pack is a base
//! or a mod, the base id a mod shadows, the layer order mods sort by, the RDT
//! and SCD dialects it carries, a free-form engine note and the explicit Lua
//! hook list.
//!
//! The parser is a deliberately small, dependency-free TOML subset: comments,
//! blank lines, `key = value` with strings, integers, booleans and string
//! arrays, plus `[section]` headers. Every malformed value names its line, so
//! an authoring typo is reported where it was written. Booleans are accepted by
//! the parser even though no current manifest field is one, so the grammar the
//! tests pin is the grammar the parser implements.

use std::collections::HashSet;

use anyhow::{Context, Result, anyhow, bail};

use crate::budget;

/// The pack entry holding a rendered manifest.
pub const ENTRY: &str = "manifest.toml";

/// The manifest format version this module writes and accepts.
pub const FORMAT: u32 = 1;

/// The default RDT dialect when a manifest does not name one.
pub const DEFAULT_RDT_VERSION: &str = "re1";

/// The default SCD dialect when a manifest does not name one.
pub const DEFAULT_SCD_VERSION: &str = "re1";

/// Whether a pack is a base game pack or a mod layered over one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackKind {
    /// The base pack: the full converted game.
    Base,
    /// A mod pack that layers over a base pack.
    Mod,
}

impl PackKind {
    /// The manifest spelling of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Mod => "mod",
        }
    }
}

impl std::fmt::Display for PackKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A parsed pack manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Manifest format version; always [`FORMAT`].
    pub format: u32,
    /// Stable pack id, e.g. `re1` or `demo`.
    pub id: String,
    /// Human-readable pack name.
    pub name: Option<String>,
    /// Pack version string.
    pub version: Option<String>,
    /// Whether this is the base pack or a mod.
    pub kind: PackKind,
    /// Mods: the base pack id this mod layers over.
    pub base: Option<String>,
    /// Mods: layer order; lower layers are applied first.
    pub load_order: i32,
    /// RDT dialect, e.g. `re1`.
    pub rdt_version: String,
    /// SCD dialect, e.g. `re1`.
    pub scd_version: String,
    /// Optional note about the engine build this pack targets.
    pub engine: Option<String>,
    /// Explicit Lua hook paths, in load order.
    pub lua: Vec<String>,
}

impl Manifest {
    /// A base manifest for `id` with the default RE1 dialects.
    pub fn base(id: impl Into<String>) -> Self {
        Self {
            format: FORMAT,
            id: id.into(),
            name: None,
            version: None,
            kind: PackKind::Base,
            base: None,
            load_order: 0,
            rdt_version: DEFAULT_RDT_VERSION.to_string(),
            scd_version: DEFAULT_SCD_VERSION.to_string(),
            engine: None,
            lua: Vec::new(),
        }
    }

    /// Whether this pack declares itself a mod.
    pub fn is_mod(&self) -> bool {
        self.kind == PackKind::Mod
    }

    /// Parse a manifest, naming the line of every malformed value.
    ///
    /// The source, every line and the statement count are capped by
    /// [`budget::MAX_TEXT_BYTES`], [`budget::MAX_MANIFEST_LINE`] and
    /// [`budget::MAX_MANIFEST_ITEMS`].
    pub fn parse(text: &str) -> Result<Self> {
        budget::check_len(text.len(), budget::MAX_TEXT_BYTES, "manifest size")?;
        let mut format = None;
        let mut id = None;
        let mut name = None;
        let mut version = None;
        let mut kind = None;
        let mut base = None;
        let mut load_order = None;
        let mut rdt_version = None;
        let mut scd_version = None;
        let mut engine = None;
        let mut lua = None;
        let mut seen: HashSet<String> = HashSet::new();

        for item in parse_items(text)? {
            let section = item.section.as_deref().unwrap_or("pack");
            if section != "pack" {
                bail!(
                    "manifest.toml line {}: unknown section [{section}]",
                    item.line
                );
            }
            if !seen.insert(item.key.clone()) {
                bail!(
                    "manifest.toml line {}: duplicate key `{}`",
                    item.line,
                    item.key
                );
            }
            let line = item.line;
            match item.key.as_str() {
                "format" => {
                    let Value::Integer(value) = item.value else {
                        bail!("manifest.toml line {line}: `format` must be an integer");
                    };
                    let value = u32::try_from(value)
                        .map_err(|_| anyhow!("manifest.toml line {line}: bad format {value}"))?;
                    if value != FORMAT {
                        bail!(
                            "manifest.toml line {line}: unsupported manifest format {value} (expected {FORMAT})"
                        );
                    }
                    format = Some(value);
                }
                "id" => id = Some(non_empty_string(item, "id")?),
                "name" => name = Some(string_value(item, "name")?),
                "version" => version = Some(string_value(item, "version")?),
                "kind" => {
                    let text = string_value(item, "kind")?;
                    kind = Some(match text.as_str() {
                        "base" => PackKind::Base,
                        "mod" => PackKind::Mod,
                        other => bail!(
                            "manifest.toml line {line}: `kind` must be \"base\" or \"mod\", found \"{other}\""
                        ),
                    });
                }
                "base" => base = Some(non_empty_string(item, "base")?),
                "load_order" => {
                    let Value::Integer(value) = item.value else {
                        bail!("manifest.toml line {line}: `load_order` must be an integer");
                    };
                    load_order = Some(i32::try_from(value).map_err(|_| {
                        anyhow!("manifest.toml line {line}: `load_order` does not fit an i32")
                    })?);
                }
                "rdt" => rdt_version = Some(non_empty_string(item, "rdt")?),
                "scd" => scd_version = Some(non_empty_string(item, "scd")?),
                "engine" => engine = Some(string_value(item, "engine")?),
                "lua" => {
                    let Value::Array(values) = item.value else {
                        bail!("manifest.toml line {line}: `lua` must be a string array");
                    };
                    budget::check_len(
                        values.len(),
                        budget::MAX_MANIFEST_LUA,
                        "manifest lua list length",
                    )?;
                    if values.iter().any(String::is_empty) {
                        bail!("manifest.toml line {line}: `lua` entries must not be empty");
                    }
                    lua = Some(values);
                }
                unknown => bail!("manifest.toml line {line}: unknown key `{unknown}`"),
            }
        }

        let format = format.context("manifest.toml: missing `format`")?;
        let id = id.context("manifest.toml: missing `id`")?;
        let kind = kind.context("manifest.toml: missing `kind`")?;
        if kind == PackKind::Mod && base.is_none() {
            bail!("manifest.toml: a mod manifest must name a `base`");
        }

        Ok(Self {
            format,
            id,
            name,
            version,
            kind,
            base,
            load_order: load_order.unwrap_or(0),
            rdt_version: rdt_version.unwrap_or_else(|| DEFAULT_RDT_VERSION.to_string()),
            scd_version: scd_version.unwrap_or_else(|| DEFAULT_SCD_VERSION.to_string()),
            engine,
            lua: lua.unwrap_or_default(),
        })
    }

    /// Render the manifest deterministically.
    ///
    /// The output is the canonical checked-in form: a `[pack]` section, one
    /// `key = value` line per set field in a fixed order, and a trailing
    /// newline. Parsing the result yields the same manifest.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("[pack]\n");
        write_pair(&mut out, "format", &self.format.to_string());
        write_pair(&mut out, "id", &quote(&self.id));
        if let Some(name) = &self.name {
            write_pair(&mut out, "name", &quote(name));
        }
        if let Some(version) = &self.version {
            write_pair(&mut out, "version", &quote(version));
        }
        write_pair(&mut out, "kind", &quote(self.kind.as_str()));
        if let Some(base) = &self.base {
            write_pair(&mut out, "base", &quote(base));
        }
        if self.kind == PackKind::Mod || self.load_order != 0 {
            write_pair(&mut out, "load_order", &self.load_order.to_string());
        }
        write_pair(&mut out, "rdt", &quote(&self.rdt_version));
        write_pair(&mut out, "scd", &quote(&self.scd_version));
        if let Some(engine) = &self.engine {
            write_pair(&mut out, "engine", &quote(engine));
        }
        if !self.lua.is_empty() {
            let entries: Vec<String> = self.lua.iter().map(|path| quote(path)).collect();
            write_pair(&mut out, "lua", &format!("[{}]", entries.join(", ")));
        }
        out
    }
}

/// The value kinds the subset grammar can represent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    /// A double-quoted string.
    String(String),
    /// A signed integer.
    Integer(i64),
    /// `true` or `false`.
    Bool(bool),
    /// A bracketed list of strings.
    Array(Vec<String>),
}

/// One parsed `key = value` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    /// Enclosing section, if any.
    section: Option<String>,
    /// The key.
    key: String,
    /// The parsed value.
    value: Value,
    /// 1-based source line.
    line: usize,
}

/// Parse the subset grammar into a flat list of statements.
fn parse_items(text: &str) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut section: Option<String> = None;

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        budget::check_len(
            raw.len(),
            budget::MAX_MANIFEST_LINE,
            &format!("manifest.toml line {line} length"),
        )?;
        let content = strip_comment(raw).trim();
        if content.is_empty() {
            continue;
        }

        if let Some(inner) = content.strip_prefix('[') {
            let Some(name) = inner.strip_suffix(']') else {
                bail!("manifest.toml line {line}: malformed section header");
            };
            let name = name.trim();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
            {
                bail!("manifest.toml line {line}: invalid section name `{name}`");
            }
            section = Some(name.to_string());
            continue;
        }

        let (key, value) = split_key_value(content, line)?;
        if !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        {
            bail!("manifest.toml line {line}: invalid key `{key}`");
        }
        if !seen.insert((section.clone(), key.to_string())) {
            bail!("manifest.toml line {line}: duplicate key `{key}`");
        }
        budget::check_len(
            items.len() + 1,
            budget::MAX_MANIFEST_ITEMS,
            "manifest statement count",
        )?;
        items.push(Item {
            section: section.clone(),
            key: key.to_string(),
            value: parse_value(value, line)?,
            line,
        });
    }

    Ok(items)
}

/// Remove a `#` comment that starts outside a string.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        } else if ch == '#' {
            return &line[..index];
        }
    }
    line
}

/// Split `key = value` at the first unquoted `=`.
fn split_key_value(text: &str, line: usize) -> Result<(&str, &str)> {
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
        } else if ch == '=' {
            let key = text[..index].trim();
            let value = text[index + 1..].trim();
            if key.is_empty() {
                bail!("manifest.toml line {line}: missing key before `=`");
            }
            if value.is_empty() {
                bail!("manifest.toml line {line}: missing value for `{key}`");
            }
            return Ok((key, value));
        }
    }
    bail!("manifest.toml line {line}: expected `key = value`")
}

/// Parse one value.
fn parse_value(text: &str, line: usize) -> Result<Value> {
    if text.starts_with('"') {
        return Ok(Value::String(parse_string(text, line)?));
    }
    if text.starts_with('[') {
        return Ok(Value::Array(parse_array(text, line)?));
    }
    match text {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ => {
            let value = text.parse::<i64>().map_err(|_| {
                anyhow!(
                    "manifest.toml line {line}: expected a string, integer, bool or string array, found `{text}`"
                )
            })?;
            Ok(Value::Integer(value))
        }
    }
}

/// Parse a standalone double-quoted string.
fn parse_string(text: &str, line: usize) -> Result<String> {
    let (value, consumed) = parse_quoted(text, line)?;
    let trailing = text[consumed..].trim();
    if !trailing.is_empty() {
        bail!("manifest.toml line {line}: unexpected trailing text `{trailing}` after string");
    }
    Ok(value)
}

/// Parse a double-quoted string at the start of `text`, returning the decoded
/// value and the number of bytes consumed.
fn parse_quoted(text: &str, line: usize) -> Result<(String, usize)> {
    let mut chars = text.char_indices().peekable();
    if !matches!(chars.peek(), Some((_, '"'))) {
        bail!("manifest.toml line {line}: expected a double-quoted string");
    }
    chars.next();
    let mut value = String::new();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => return Ok((value, index + 1)),
            '\\' => {
                let Some((_, escaped)) = chars.next() else {
                    bail!("manifest.toml line {line}: unterminated string");
                };
                match escaped {
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    other => {
                        bail!("manifest.toml line {line}: unsupported escape `\\{other}`")
                    }
                }
            }
            other => value.push(other),
        }
    }
    bail!("manifest.toml line {line}: unterminated string")
}

/// Parse a bracketed string array.
fn parse_array(text: &str, line: usize) -> Result<Vec<String>> {
    let mut rest = text
        .strip_prefix('[')
        .expect("callers only pass `[`-prefixed text")
        .trim_start();
    let mut values = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix(']') {
            let trailing = after.trim();
            if !trailing.is_empty() {
                bail!(
                    "manifest.toml line {line}: unexpected trailing text `{trailing}` after array"
                );
            }
            return Ok(values);
        }
        if rest.is_empty() {
            bail!("manifest.toml line {line}: unterminated array");
        }
        let (value, consumed) = parse_quoted(rest, line)?;
        budget::check_len(
            values.len() + 1,
            budget::MAX_MANIFEST_ITEMS,
            "manifest array length",
        )?;
        values.push(value);
        rest = rest[consumed..].trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after.trim_start();
        } else if !rest.starts_with(']') {
            bail!("manifest.toml line {line}: expected `,` or `]` in array");
        }
    }
}

/// Extract a required string manifest field.
fn string_value(item: Item, key: &str) -> Result<String> {
    match item.value {
        Value::String(value) => Ok(value),
        _ => bail!("manifest.toml line {}: `{key}` must be a string", item.line),
    }
}

/// Extract a required non-empty string manifest field.
fn non_empty_string(item: Item, key: &str) -> Result<String> {
    let line = item.line;
    let value = string_value(item, key)?;
    if value.is_empty() {
        bail!("manifest.toml line {line}: `{key}` must not be empty");
    }
    Ok(value)
}

/// Append one rendered `key = value` line.
fn write_pair(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(value);
    out.push('\n');
}

/// Quote and escape a string for rendering.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_manifest() -> Manifest {
        Manifest {
            format: FORMAT,
            id: "re1".to_string(),
            name: Some("Arklay base".to_string()),
            version: Some("0.1.0".to_string()),
            kind: PackKind::Base,
            base: None,
            load_order: 0,
            rdt_version: DEFAULT_RDT_VERSION.to_string(),
            scd_version: DEFAULT_SCD_VERSION.to_string(),
            engine: Some("arklay 0.1.0".to_string()),
            lua: Vec::new(),
        }
    }

    fn mod_manifest() -> Manifest {
        Manifest {
            format: FORMAT,
            id: "demo".to_string(),
            name: Some("Demo mod".to_string()),
            version: None,
            kind: PackKind::Mod,
            base: Some("re1".to_string()),
            load_order: 40,
            rdt_version: DEFAULT_RDT_VERSION.to_string(),
            scd_version: DEFAULT_SCD_VERSION.to_string(),
            engine: None,
            lua: vec!["lua/demo.lua".to_string(), "lua/extra.lua".to_string()],
        }
    }

    #[test]
    fn parses_every_value_kind() {
        let items = parse_items(
            r#"
# a leading comment
name = "demo"   # trailing comment
count = -3
enabled = true
off = false
files = ["a.lua", "b#c.lua"]   # `#` inside a string survives
empty = []
"#,
        )
        .unwrap();

        let value = |key: &str| items.iter().find(|item| item.key == key).unwrap();
        assert_eq!(value("name").value, Value::String("demo".to_string()));
        assert_eq!(value("count").value, Value::Integer(-3));
        assert_eq!(value("enabled").value, Value::Bool(true));
        assert_eq!(value("off").value, Value::Bool(false));
        assert_eq!(
            value("files").value,
            Value::Array(vec!["a.lua".to_string(), "b#c.lua".to_string()])
        );
        assert_eq!(value("empty").value, Value::Array(Vec::new()));
        assert_eq!(value("name").line, 3);
    }

    #[test]
    fn parses_sections_and_string_escapes() {
        let items = parse_items("[pack]\nid = \"a\\\"b\"\n\n[lua]\npath = \"x\\ny\"\n").unwrap();

        assert_eq!(items[0].section.as_deref(), Some("pack"));
        assert_eq!(items[0].value, Value::String("a\"b".to_string()));
        assert_eq!(items[1].section.as_deref(), Some("lua"));
        assert_eq!(items[1].value, Value::String("x\ny".to_string()));
    }

    #[test]
    fn parse_errors_name_the_line() {
        let cases = [
            ("x = \"unterminated", "line 1"),
            ("x = \"a\" tail", "trailing text"),
            ("x = \"\\q\"", "unsupported escape"),
            ("x = [\"a\" \"b\"]", "expected `,` or `]`"),
            ("x = [\"a\",", "unterminated array"),
            ("x = 12abc", "found `12abc`"),
            ("x =", "missing value"),
            ("= 1", "missing key"),
            ("just words", "expected `key = value`"),
            ("[pack", "malformed section header"),
            ("[]", "invalid section name"),
            ("[weird!]\nx = 1", "invalid section name"),
            ("a = 1\na = 2", "duplicate key"),
            ("x! = 1", "invalid key"),
        ];
        for (text, want) in cases {
            let err = parse_items(text).unwrap_err().to_string();
            assert!(err.contains(want), "{text:?}: {err}");
            assert!(err.contains("line"), "{text:?}: {err}");
        }
    }

    #[test]
    fn parse_manifest_maps_all_fields() {
        let text = "\
[pack]
format = 1
id = \"demo\"
name = \"Demo mod\"
version = \"1.2.3\"
kind = \"mod\"
base = \"re1\"
load_order = -7
rdt = \"re1-jpn\"
scd = \"re1\"
engine = \"arklay 0.1.0\"
lua = [\"lua/a.lua\", \"lua/b.lua\"]
";
        assert_eq!(Manifest::parse(text).unwrap(), mod_manifest_with_extra());
    }

    fn mod_manifest_with_extra() -> Manifest {
        Manifest {
            format: FORMAT,
            id: "demo".to_string(),
            name: Some("Demo mod".to_string()),
            version: Some("1.2.3".to_string()),
            kind: PackKind::Mod,
            base: Some("re1".to_string()),
            load_order: -7,
            rdt_version: "re1-jpn".to_string(),
            scd_version: "re1".to_string(),
            engine: Some("arklay 0.1.0".to_string()),
            lua: vec!["lua/a.lua".to_string(), "lua/b.lua".to_string()],
        }
    }

    #[test]
    fn parse_defaults_the_dialects_and_load_order() {
        let manifest = Manifest::parse("format = 1\nid = \"re1\"\nkind = \"base\"\n").unwrap();
        assert_eq!(manifest.rdt_version, "re1");
        assert_eq!(manifest.scd_version, "re1");
        assert_eq!(manifest.load_order, 0);
        assert_eq!(manifest.lua, Vec::<String>::new());
        assert_eq!(manifest.name, None);
        assert_eq!(manifest.version, None);
        assert_eq!(manifest.engine, None);
    }

    #[test]
    fn missing_and_unknown_fields_are_errors() {
        let cases = [
            ("id = \"x\"\nkind = \"base\"", "missing `format`"),
            ("format = 1\nkind = \"base\"", "missing `id`"),
            ("format = 1\nid = \"x\"", "missing `kind`"),
            (
                "format = 2\nid = \"x\"\nkind = \"base\"",
                "unsupported manifest format 2",
            ),
            (
                "format = 1\nid = \"x\"\nkind = \"mod\"",
                "a mod manifest must name a `base`",
            ),
            (
                "format = 1\nid = \"\"\nkind = \"base\"",
                "`id` must not be empty",
            ),
            (
                "format = 1\nid = \"x\"\nkind = \"dll\"",
                "`kind` must be \"base\" or \"mod\"",
            ),
            (
                "format = 1\nid = \"x\"\nkind = \"base\"\nunknown = 1",
                "unknown key `unknown`",
            ),
            (
                "format = 1\nid = \"x\"\nkind = \"base\"\n[other]\nx = 1",
                "unknown section [other]",
            ),
            (
                "format = 1\nid = \"x\"\nkind = \"base\"\nformat = 1",
                "duplicate key `format`",
            ),
            (
                "format = 1\nid = \"x\"\nkind = \"base\"\nlua = [\"\"]",
                "`lua` entries must not be empty",
            ),
            (
                "format = 1\nid = \"x\"\nkind = \"base\"\nload_order = 999999999999",
                "does not fit an i32",
            ),
        ];
        for (text, want) in cases {
            let err = Manifest::parse(text).unwrap_err().to_string();
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }

    #[test]
    fn rejects_text_over_the_caps() {
        let long_line = format!("id = \"{}\"\n", "a".repeat(budget::MAX_MANIFEST_LINE));
        let message = budget::assert_cap_error(Manifest::parse(&long_line));
        assert!(message.contains("line 1 length"), "{message}");

        let mut many = String::new();
        for index in 0..=budget::MAX_MANIFEST_ITEMS {
            many.push_str(&format!("k{index} = 1\n"));
        }
        let message = budget::assert_cap_error(Manifest::parse(&many));
        assert!(message.contains("statement count"), "{message}");

        let entries: Vec<&str> =
            std::iter::repeat_n("\"lua/a.lua\"", budget::MAX_MANIFEST_LUA + 1).collect();
        let text = format!(
            "format = 1\nid = \"x\"\nkind = \"base\"\nlua = [{}]\n",
            entries.join(", ")
        );
        let message = budget::assert_cap_error(Manifest::parse(&text));
        assert!(message.contains("lua list length"), "{message}");
    }

    #[test]
    fn render_matches_the_canonical_form() {
        assert_eq!(
            base_manifest().render(),
            "\
[pack]
format = 1
id = \"re1\"
name = \"Arklay base\"
version = \"0.1.0\"
kind = \"base\"
rdt = \"re1\"
scd = \"re1\"
engine = \"arklay 0.1.0\"
"
        );
        assert_eq!(
            mod_manifest().render(),
            "\
[pack]
format = 1
id = \"demo\"
name = \"Demo mod\"
kind = \"mod\"
base = \"re1\"
load_order = 40
rdt = \"re1\"
scd = \"re1\"
lua = [\"lua/demo.lua\", \"lua/extra.lua\"]
"
        );
    }

    #[test]
    fn render_escapes_special_characters() {
        let manifest = Manifest {
            name: Some("a \"b\" \\ c\nd".to_string()),
            ..base_manifest()
        };
        let rendered = manifest.render();
        assert!(
            rendered.contains("name = \"a \\\"b\\\" \\\\ c\\nd\""),
            "{rendered}"
        );
        assert_eq!(Manifest::parse(&rendered).unwrap(), manifest);
    }

    #[test]
    fn render_round_trips_every_parsed_manifest() {
        for source in [
            "format = 1\nid = \"re1\"\nkind = \"base\"\n",
            "\
[pack]
format = 1
id = \"demo\"
name = \"Demo\"
version = \"2\"
kind = \"mod\"
base = \"re1\"
load_order = 5
lua = [\"lua/a.lua\"]
",
        ] {
            let manifest = Manifest::parse(source).unwrap();
            let rendered = manifest.render();
            assert_eq!(Manifest::parse(&rendered).unwrap(), manifest);
            assert_eq!(rendered, manifest.render(), "render must be deterministic");
        }
    }

    #[test]
    fn pack_kind_renders_its_spelling() {
        assert_eq!(PackKind::Base.as_str(), "base");
        assert_eq!(PackKind::Mod.to_string(), "mod");
    }
}
