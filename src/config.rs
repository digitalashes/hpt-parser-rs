//! The output config: which columns a record becomes, from which fields, with which
//! transforms, cast to which type. Written in TOML:
//!
//! ```toml
//! [output]
//! compression = "zstd:3"
//!
//! [[column]]
//! name = "negotiated"
//! from = "payer.dollar"
//! type = "f64"
//! transform = ["money", { round = 2 }]
//! ```
//!
//! Each column reads one field (`charge.description`, `payer.dollar`,
//! `metadata.hospital_name`, ...; [`Field::names`] lists them) or several, whose texts are
//! joined by `|` (useful for `hash`). The transforms run in order, then the cast to the
//! column's type; a value that does not cast is null.
//!
//! Transforms: `trim`, `upper`, `lower`, `money` (drops `$`, `,` and spaces), `{ round = N }`,
//! `{ map = { from = "to" } }`, `{ match = "<domain>", fuzzy = 0.8 }` (the CMS values of
//! `setting`, `methodology`, `code_type`, `drug_unit` or `billing_class`), `{ split = "|" }`,
//! `{ default = "x" }`, `codes_by_type`, `codes_list`, `hash`, `{ plugin = "name.fn" }`.

use crate::text::{is_space, split, trim};
use crate::{Code, Error, Layout, Metadata, Record};
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// The built-in config: every record field as a column of its own, unchanged.
pub const BUILTIN: &str = include_str!("default.toml");

#[derive(Clone, Debug)]
pub struct Config {
    pub output: Output,
    pub plugins: Vec<Plugin>,
    pub columns: Vec<Column>,
    pub filters: Vec<Filter>,
}

/// `[output]`: kept as written, read by the writer.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    /// `none`, `snappy`, `gzip[:level]`, `lz4`, `zstd[:level]`, `brotli[:level]`.
    pub compression: Option<String>,
    pub row_group_size: Option<usize>,
    /// Records per output file, 0 for one file.
    pub chunk_size: Option<u64>,
}

/// `[[plugin]]`: a WASM module, its functions named `<name>.<function>`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub name: String,
    pub path: PathBuf,
    /// Handed to the plugin.
    #[serde(default)]
    pub config: BTreeMap<String, toml::Value>,
}

/// `[[filter]]`: a plugin function that keeps or drops each record.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub plugin: String,
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub source: Source,
    pub kind: Type,
    pub transforms: Vec<Transform>,
}

#[derive(Clone, Debug)]
pub enum Source {
    Fields(Vec<Field>),
    /// A plugin function computes the value.
    Plugin(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Type {
    String,
    F64,
    I64,
    Bool,
    /// `YYYY-MM-DD` or `M/D/YYYY`, as days since 1970-01-01.
    Date,
    /// JSON text: a list or the codes as an array, other text as a string.
    Json,
    List,
}

#[derive(Clone, Debug)]
pub enum Transform {
    Trim,
    Upper,
    Lower,
    Money,
    Round(usize),
    /// Exact match after trimming, then case-insensitive.
    Map(Vec<(String, String)>),
    /// A domain's values, the `fuzzy` similarity.
    Match(&'static [&'static str], Option<f64>),
    Split(String),
    /// For a null or empty value.
    Default(String),
    CodesByType,
    CodesList,
    Hash,
    Plugin(String),
}

/// The CMS value domains for `match`.
const DOMAINS: &[(&str, &[&str])] = &[
    ("setting", &["inpatient", "outpatient", "both"]),
    (
        "methodology",
        &["case rate", "fee schedule", "percent of total billed charges", "per diem", "other"],
    ),
    (
        "code_type",
        &[
            "CPT", "HCPCS", "ICD", "DRG", "MS-DRG", "R-DRG", "S-DRG", "APS-DRG", "AP-DRG",
            "APR-DRG", "TRIS-DRG", "APC", "NDC", "HIPPS", "LOCAL", "EAPG", "CDT", "RC", "CDM",
            "CMG", "MS-LTC-DRG",
        ],
    ),
    ("drug_unit", &["GR", "ME", "ML", "UN", "F2", "EA", "GM"]),
    ("billing_class", &["professional", "facility", "both"]),
];

/// A column's value, handed to the writer.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Text(String),
    List(Vec<String>),
    F64(f64),
    I64(i64),
    Bool(bool),
    /// Days since 1970-01-01.
    Date(i32),
}

/// The TOML as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    output: Output,
    #[serde(default)]
    plugin: Vec<Plugin>,
    #[serde(default)]
    column: Vec<RawColumn>,
    #[serde(default)]
    filter: Vec<Filter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawColumn {
    name: String,
    from: Option<Names>,
    plugin: Option<String>,
    r#type: Type,
    #[serde(default)]
    transform: Vec<toml::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Names {
    One(String),
    Many(Vec<String>),
}

impl Config {
    /// # Errors
    ///
    /// [`Error::Config`] when the file cannot be read or does not load.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let error = |message| Error::Config { message: format!("{}: {message}", path.display()) };
        let text = std::fs::read_to_string(path).map_err(|e| error(e.to_string()))?;
        Self::parse(&text).map_err(|e| error(e.to_string()))
    }

    /// # Errors
    ///
    /// [`Error::Config`] for TOML that does not parse, an unknown key, field, transform or
    /// domain, a column defined twice or a plugin function of an undeclared plugin.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let error = |message: String| Error::Config { message };
        let raw: Raw = toml::from_str(text).map_err(|e| error(e.to_string().trim_end().into()))?;
        let mut names = HashSet::new();
        let mut columns = Vec::new();
        for c in raw.column {
            let name = c.name.clone();
            if !names.insert(c.name.clone()) {
                return Err(error(format!("column `{name}` is defined twice")));
            }
            let column = column(c, &raw.plugin);
            columns.push(column.map_err(|e| error(format!("column `{name}`: {e}")))?);
        }
        for f in &raw.filter {
            plugin_function(&f.plugin, &raw.plugin).map_err(|e| error(format!("filter: {e}")))?;
        }
        Ok(Config { output: raw.output, plugins: raw.plugin, columns, filters: raw.filter })
    }

    /// # Panics
    ///
    /// Never: a test loads it.
    #[must_use]
    pub fn builtin() -> Self {
        Self::parse(BUILTIN).expect("the built-in config is tested")
    }
}

fn column(c: RawColumn, plugins: &[Plugin]) -> Result<Column, String> {
    let source = match (c.from, &c.plugin) {
        (Some(from), None) => {
            let names = match from {
                Names::One(name) => vec![name],
                Names::Many(names) => names,
            };
            if names.is_empty() {
                return Err("`from` names no field".into());
            }
            let fields = names
                .iter()
                .map(|name| Field::parse(name).ok_or_else(|| format!("unknown field `{name}`")));
            Source::Fields(fields.collect::<Result<_, _>>()?)
        }
        (None, Some(plugin)) => Source::Plugin(plugin_function(plugin, plugins)?),
        _ => return Err("needs one of `from` and `plugin`".into()),
    };
    let transforms = c.transform.iter().map(|t| transform(t, plugins));
    let transforms = transforms.collect::<Result<_, _>>()?;
    Ok(Column { name: c.name, source, kind: c.r#type, transforms })
}

/// `<plugin>.<function>`, the plugin declared.
fn plugin_function(name: &str, plugins: &[Plugin]) -> Result<String, String> {
    match name.split_once('.') {
        Some((plugin, function)) if !function.is_empty() => {
            if plugins.iter().any(|p| p.name == plugin) {
                Ok(name.into())
            } else {
                Err(format!("plugin `{plugin}` is not declared in a [[plugin]] table"))
            }
        }
        _ => Err(format!("`{name}` is not `<plugin>.<function>`")),
    }
}

fn transform(t: &toml::Value, plugins: &[Plugin]) -> Result<Transform, String> {
    let table = match t {
        toml::Value::String(name) => {
            return Ok(match name.as_str() {
                "trim" => Transform::Trim,
                "upper" => Transform::Upper,
                "lower" => Transform::Lower,
                "money" => Transform::Money,
                "codes_by_type" => Transform::CodesByType,
                "codes_list" => Transform::CodesList,
                "hash" => Transform::Hash,
                _ => return Err(format!("unknown transform `{name}`")),
            });
        }
        toml::Value::Table(table) => table,
        _ => return Err(format!("a transform is a name or a table, not `{t}`")),
    };
    // `fuzzy` goes with `match`; every other table has one key.
    let mut keys = table.keys().filter(|k| *k != "fuzzy" || !table.contains_key("match"));
    let (Some(name), None) = (keys.next(), keys.next()) else {
        return Err(format!("a transform table has one key: `{t}`"));
    };
    let text = |v: &toml::Value| v.as_str().map(String::from);
    Ok(match (name.as_str(), &table[name]) {
        ("round", toml::Value::Integer(n)) => {
            let digits = usize::try_from(*n).ok().filter(|&n| n <= 17);
            Transform::Round(digits.ok_or("`round` takes 0 to 17 digits")?)
        }
        ("map", toml::Value::Table(map)) => {
            let pairs = map.iter().map(|(from, to)| {
                text(to)
                    .map(|to| (from.clone(), to))
                    .ok_or(format!("`map` value of `{from}` is not text"))
            });
            Transform::Map(pairs.collect::<Result<_, _>>()?)
        }
        ("match", toml::Value::String(domain)) => {
            let values = (DOMAINS.iter().find(|(name, _)| name == domain))
                .ok_or(format!("unknown domain `{domain}`"))?
                .1;
            let fuzzy = match table.get("fuzzy") {
                None => None,
                Some(f) => Some(
                    (f.as_float())
                        .or_else(|| {
                            f.as_integer().and_then(|n| i32::try_from(n).ok()).map(f64::from)
                        })
                        .filter(|f| (0.0..=1.0).contains(f))
                        .ok_or("`fuzzy` is a similarity from 0 to 1")?,
                ),
            };
            Transform::Match(values, fuzzy)
        }
        ("split", toml::Value::String(s)) => Transform::Split(s.clone()),
        ("default", toml::Value::String(s)) => Transform::Default(s.clone()),
        ("plugin", toml::Value::String(s)) => Transform::Plugin(plugin_function(s, plugins)?),
        (name, value) => return Err(format!("unknown transform or argument `{name} = {value}`")),
    })
}

/// A field a column reads.
#[derive(Clone)]
pub struct Field(Name);

#[derive(Clone)]
enum Name {
    /// An index into `FIELDS`.
    Known(usize),
    /// `metadata.extra.<key>`.
    Extra(String),
}

type Get = for<'a> fn(&'a Record, &'a Metadata) -> Work<'a>;

fn text(v: Option<&str>) -> Work<'_> {
    v.map_or(Work::Null, |v| Work::Text(Cow::Borrowed(v)))
}

/// Every field but `metadata.extra.<key>`, in the order of the built-in config.
const FIELDS: &[(&str, Get)] = &[
    ("charge.description", |r, _| text(r.charge.description.as_deref())),
    ("charge.codes", |r, _| Work::Codes(&r.charge.codes)),
    ("charge.setting", |r, _| text(r.charge.setting.as_deref())),
    ("charge.billing_class", |r, _| text(r.charge.billing_class.as_deref())),
    ("charge.drug_unit", |r, _| text(r.charge.drug_unit.as_deref())),
    ("charge.drug_unit_type", |r, _| text(r.charge.drug_unit_type.as_deref())),
    ("charge.modifiers", |r, _| Work::List(r.charge.modifiers.clone())),
    ("charge.gross_charge", |r, _| text(r.charge.gross_charge.as_deref())),
    ("charge.discounted_cash", |r, _| text(r.charge.discounted_cash.as_deref())),
    ("charge.minimum", |r, _| text(r.charge.minimum.as_deref())),
    ("charge.maximum", |r, _| text(r.charge.maximum.as_deref())),
    ("charge.notes", |r, _| text(r.charge.notes.as_deref())),
    ("payer.name", |r, _| text(payer(r, |p| p.name.as_deref()))),
    ("payer.plan", |r, _| text(payer(r, |p| p.plan.as_deref()))),
    ("payer.dollar", |r, _| text(payer(r, |p| p.dollar.as_deref()))),
    ("payer.percentage", |r, _| text(payer(r, |p| p.percentage.as_deref()))),
    ("payer.algorithm", |r, _| text(payer(r, |p| p.algorithm.as_deref()))),
    ("payer.methodology", |r, _| text(payer(r, |p| p.methodology.as_deref()))),
    ("payer.estimated_amount", |r, _| text(payer(r, |p| p.estimated_amount.as_deref()))),
    ("payer.median_amount", |r, _| text(payer(r, |p| p.median_amount.as_deref()))),
    ("payer.percentile_10th", |r, _| text(payer(r, |p| p.percentile_10th.as_deref()))),
    ("payer.percentile_90th", |r, _| text(payer(r, |p| p.percentile_90th.as_deref()))),
    ("payer.count", |r, _| text(payer(r, |p| p.count.as_deref()))),
    ("payer.notes", |r, _| text(payer(r, |p| p.notes.as_deref()))),
    ("record.position", |r, _| Work::Text(Cow::Owned(r.position.to_string()))),
    ("metadata.layout", |_, m| text(m.layout.map(Layout::name))),
    ("metadata.version", |_, m| text(m.version.as_deref())),
    ("metadata.hospital_name", |_, m| text(m.hospital_name.as_deref())),
    ("metadata.last_updated_on", |_, m| text(m.last_updated_on.as_deref())),
    ("metadata.location_name", |_, m| Work::List(m.location_name.clone())),
    ("metadata.hospital_address", |_, m| Work::List(m.hospital_address.clone())),
    ("metadata.license_number", |_, m| text(m.license_number.as_deref())),
    ("metadata.license_state", |_, m| text(m.license_state.as_deref())),
    ("metadata.type_2_npi", |_, m| Work::List(m.type_2_npi.clone())),
    ("metadata.attestation", |_, m| text(m.attestation.as_deref())),
    ("metadata.attestation_confirmed", |_, m| text(m.attestation_confirmed.as_deref())),
    ("metadata.attester_name", |_, m| text(m.attester_name.as_deref())),
];

fn payer(r: &Record, get: fn(&crate::Payer) -> Option<&str>) -> Option<&str> {
    r.payer.as_ref().and_then(get)
}

impl Field {
    fn parse(name: &str) -> Option<Self> {
        if let Some(key) = name.strip_prefix("metadata.extra.") {
            return Some(Field(Name::Extra(key.into())));
        }
        FIELDS.iter().position(|(n, _)| *n == name).map(|i| Field(Name::Known(i)))
    }

    /// Every field name but `metadata.extra.<key>`.
    pub fn names() -> impl Iterator<Item = &'static str> {
        FIELDS.iter().map(|(name, _)| *name)
    }

    fn get<'a>(&self, record: &'a Record, metadata: &'a Metadata) -> Work<'a> {
        match &self.0 {
            Name::Known(i) => (FIELDS[*i].1)(record, metadata),
            Name::Extra(key) => text(metadata.extra.get(key).map(String::as_str)),
        }
    }
}

impl fmt::Debug for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Name::Known(i) => f.write_str(FIELDS[*i].0),
            Name::Extra(key) => write!(f, "metadata.extra.{key}"),
        }
    }
}

/// A value between transforms.
enum Work<'a> {
    Null,
    Text(Cow<'a, str>),
    List(Vec<String>),
    Codes(&'a [Code]),
    /// JSON text, from `codes_by_type`.
    Json(String),
}

impl Column {
    /// The column's value for one record. A plugin column is null and a plugin transform
    /// passes the value on: plugins are called by the writer (feature `plugins`).
    pub fn value(&self, record: &Record, metadata: &Metadata) -> Value {
        let Source::Fields(fields) = &self.source else {
            return Value::Null;
        };
        let mut work = match &fields[..] {
            [field] => field.get(record, metadata),
            fields => {
                let cells: Vec<_> = fields.iter().map(|f| flat(f.get(record, metadata))).collect();
                if cells.iter().all(Option::is_none) {
                    Work::Null
                } else {
                    let cells: Vec<_> = cells.iter().map(|c| c.as_deref().unwrap_or("")).collect();
                    Work::Text(Cow::Owned(cells.join("|")))
                }
            }
        };
        for t in &self.transforms {
            work = apply(t, work);
        }
        cast(work, self.kind)
    }
}

/// A value as one text: lists joined by `|`, codes as `codes_list` gives them.
fn flat(work: Work<'_>) -> Option<Cow<'_, str>> {
    match work {
        Work::Null => None,
        Work::Text(s) => Some(s),
        Work::Json(s) => Some(Cow::Owned(s)),
        Work::List(list) => Some(Cow::Owned(list.join("|"))),
        Work::Codes(codes) => Some(Cow::Owned(codes_list(codes).join("|"))),
    }
}

fn apply<'a>(t: &Transform, work: Work<'a>) -> Work<'a> {
    let each = |work: Work<'a>, f: &dyn Fn(String) -> String| match work {
        Work::Text(s) => Work::Text(Cow::Owned(f(s.into_owned()))),
        Work::List(list) => Work::List(list.into_iter().map(f).collect()),
        work => work,
    };
    match t {
        Transform::Trim => each(work, &|s| trim(&s).into()),
        Transform::Upper => each(work, &|s| s.to_uppercase()),
        Transform::Lower => each(work, &|s| s.to_lowercase()),
        Transform::Money => each(work, &|mut s| {
            s.retain(|c| c != '$' && c != ',' && !is_space(c));
            s
        }),
        &Transform::Round(digits) => each(work, &|s| match trim(&s).parse::<f64>() {
            Ok(n) if n.is_finite() => format!("{n:.digits$}"),
            _ => s,
        }),
        Transform::Map(pairs) => each(work, &|s| {
            let key = trim(&s);
            let lower = key.to_lowercase();
            (pairs.iter().find(|(from, _)| from == key))
                .or_else(|| pairs.iter().find(|(from, _)| from.to_lowercase() == lower))
                .map_or(s, |(_, to)| to.clone())
        }),
        Transform::Match(values, fuzzy) => {
            each(work, &|s| matched(values, &s, *fuzzy).map_or(s, String::from))
        }
        Transform::Split(sep) => match work {
            Work::Text(s) => Work::List(split(&s, sep)),
            work => work,
        },
        Transform::Default(default) => match work {
            Work::Null => Work::Text(Cow::Owned(default.clone())),
            Work::Text(s) if s.is_empty() => Work::Text(Cow::Owned(default.clone())),
            work => work,
        },
        Transform::CodesByType => match work {
            Work::Codes(codes) => Work::Json(codes_by_type(codes)),
            work => work,
        },
        Transform::CodesList => match work {
            Work::Codes(codes) => Work::List(codes_list(codes)),
            work => work,
        },
        Transform::Hash => flat(work).map_or(Work::Null, |s| Work::Text(Cow::Owned(fnv1a_128(&s)))),
        Transform::Plugin(_) => work,
    }
}

/// The domain value equal to `s` ignoring case, or with `fuzzy` the most similar one at
/// least that similar.
fn matched(values: &'static [&'static str], s: &str, fuzzy: Option<f64>) -> Option<&'static str> {
    let s = trim(s).to_lowercase();
    // The domain values are ASCII.
    if let Some(v) = values.iter().find(|v| v.eq_ignore_ascii_case(&s)) {
        return Some(v);
    }
    let threshold = fuzzy?;
    let (best, similarity) = (values.iter())
        .map(|v| (*v, similarity(&s, &v.to_lowercase())))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    (similarity >= threshold).then_some(best)
}

/// 1 minus the Levenshtein distance over the longer length, in chars.
fn similarity(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    // Two rows of the edit-distance table.
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut row = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != cb);
            row[j + 1] = substitute.min(previous[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut previous, &mut row);
    }
    #[expect(clippy::cast_precision_loss, reason = "lengths of short domain values")]
    let distance = previous[b.len()] as f64 / longest as f64;
    1.0 - distance
}

/// `TYPE:code`, sorted; a code without a type is the code alone.
fn codes_list(codes: &[Code]) -> Vec<String> {
    let mut list: Vec<String> = (codes.iter())
        .map(|c| match &c.code_type {
            Some(t) => format!("{}:{}", t.to_uppercase(), c.code),
            None => c.code.clone(),
        })
        .collect();
    list.sort();
    list
}

/// `{"CPT":["99213"],...}`: types upper-cased, types and codes sorted, codes without a type
/// under `""`.
fn codes_by_type(codes: &[Code]) -> String {
    let mut by_type: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for c in codes {
        let t = c.code_type.as_deref().unwrap_or_default().to_uppercase();
        by_type.entry(t).or_default().push(&c.code);
    }
    for codes in by_type.values_mut() {
        codes.sort_unstable();
    }
    to_json(&by_type)
}

/// FNV-1a, 128 bits, as 32 lower-case hex digits.
fn fnv1a_128(s: &str) -> String {
    const BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let hash = s.bytes().fold(BASIS, |h, b| (h ^ u128::from(b)).wrapping_mul(PRIME));
    format!("{hash:032x}")
}

fn to_json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("strings, lists and maps of them always serialize")
}

/// `[{"code":"99213","type":"CPT"},...]`, as read.
fn codes_json(codes: &[Code]) -> String {
    #[derive(serde::Serialize)]
    struct Item<'a> {
        code: &'a str,
        #[serde(rename = "type")]
        code_type: Option<&'a str>,
    }
    let items: Vec<Item> =
        codes.iter().map(|c| Item { code: &c.code, code_type: c.code_type.as_deref() }).collect();
    to_json(&items)
}

fn cast(work: Work<'_>, kind: Type) -> Value {
    let s = match (work, kind) {
        (Work::Json(s), Type::Json) => return Value::Text(s),
        (Work::Codes(codes), Type::Json) => return Value::Text(codes_json(codes)),
        (Work::Codes(codes), kind) => return cast(Work::List(codes_list(codes)), kind),
        (Work::List(list), Type::List) => return Value::List(list),
        (Work::List(list), Type::Json) => return Value::Text(to_json(&list)),
        (Work::List(list), Type::String) => return Value::Text(list.join("|")),
        (Work::Null | Work::List(_), _) => return Value::Null,
        (Work::Text(s), _) => s,
        (Work::Json(s), _) => Cow::Owned(s),
    };
    let t = trim(&s);
    let value = match kind {
        Type::String => Some(Value::Text(s.into_owned())),
        Type::F64 => t.parse::<f64>().ok().filter(|n| n.is_finite()).map(Value::F64),
        Type::I64 => t.parse().ok().map(Value::I64),
        Type::Bool => match t.to_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => Some(Value::Bool(true)),
            "false" | "no" | "n" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        Type::Date => date(t).map(Value::Date),
        Type::Json => Some(Value::Text(to_json(&s))),
        Type::List => Some(Value::List(split(&s, "|"))),
    };
    value.unwrap_or(Value::Null)
}

/// `YYYY-MM-DD` or `M/D/YYYY` as days since 1970-01-01, the day checked against the month.
fn date(s: &str) -> Option<i32> {
    let digits = |p: &str, len: std::ops::RangeInclusive<usize>| {
        (len.contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<i64>().ok())?
    };
    let (y, m, d) = match s.split('-').collect::<Vec<_>>()[..] {
        [y, m, d] => (digits(y, 4..=4)?, digits(m, 2..=2)?, digits(d, 2..=2)?),
        _ => match s.split('/').collect::<Vec<_>>()[..] {
            [m, d, y] => (digits(y, 4..=4)?, digits(m, 1..=2)?, digits(d, 1..=2)?),
            _ => return None,
        },
    };
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_month = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&d) {
        return None;
    }
    // Days from civil (H. Hinnant): years start in March, so the leap day comes last.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i32::try_from(era * 146_097 + day_of_era - 719_468).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Charge, Payer};
    use std::sync::Arc;

    fn column(toml: &str) -> Column {
        let config = Config::parse(&format!(
            "[[plugin]]\nname = \"p\"\npath = \"p.wasm\"\n[[column]]\nname = \"c\"\n{toml}"
        ))
        .unwrap();
        config.columns.into_iter().next().unwrap()
    }

    fn record(dollar: &str) -> Record {
        let charge = Charge {
            codes: vec![
                Code { code: "99213".into(), code_type: Some("cpt".into()) },
                Code { code: "0450".into(), code_type: Some("RC".into()) },
                Code { code: "X1".into(), code_type: None },
            ],
            setting: Some(dollar.into()),
            modifiers: vec!["50".into(), "LT".into()],
            ..Charge::default()
        };
        let payer = Payer { dollar: Some(dollar.into()), ..Payer::default() };
        Record { charge: Arc::new(charge), payer: Some(payer), position: 7 }
    }

    /// The column over `value` in `payer.dollar`.
    fn run(kind: &str, transform: &str, value: &str) -> Value {
        let c = column(&format!(
            "from = \"payer.dollar\"\ntype = \"{kind}\"\ntransform = [{transform}]"
        ));
        c.value(&record(value), &Metadata::default())
    }

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn text_transforms() {
        assert_eq!(run("string", "\"money\"", "$1,234.50"), t("1234.50"));
        assert_eq!(run("f64", "\"money\"", "$1,234.50"), Value::F64(1234.5));
        assert_eq!(run("f64", "\"money\"", "1,234 USD"), Value::Null);
        assert_eq!(run("string", "\"trim\", \"upper\"", " ppo "), t("PPO"));
        assert_eq!(run("string", "\"lower\"", "PPO"), t("ppo"));
        assert_eq!(run("string", "{ round = 2 }", "1e3"), t("1000.00"));
        assert_eq!(run("string", "{ round = 0 }", "2.5"), t("2"));
        assert_eq!(run("string", "{ round = 2 }", "abc"), t("abc"));
        let map = "{ map = { ip = \"inpatient\", OP = \"outpatient\" } }";
        assert_eq!(run("string", map, " ip "), t("inpatient"));
        assert_eq!(run("string", map, "op"), t("outpatient"));
        assert_eq!(run("string", map, "both"), t("both"));
        assert_eq!(run("string", "{ default = \"none\" }", ""), t("none"));
        assert_eq!(run("string", "{ default = \"none\" }", "x"), t("x"));
        assert_eq!(
            run("list", "{ split = \";\" }", "a; b;;c"),
            Value::List(vec!["a".into(), "b".into(), "c".into()])
        );
    }

    #[test]
    fn domains() {
        let exact = "{ match = \"setting\" }";
        let fuzzy = "{ match = \"setting\", fuzzy = 0.8 }";
        assert_eq!(run("string", exact, " Inpatient"), t("inpatient"));
        assert_eq!(run("string", exact, "inpatien"), t("inpatien"));
        assert_eq!(run("string", fuzzy, "inpatien"), t("inpatient"));
        assert_eq!(run("string", fuzzy, "patient"), t("patient"));
        assert_eq!(run("string", "{ match = \"code_type\" }", "ms-drg"), t("MS-DRG"));
        assert_eq!(
            run("string", "{ match = \"methodology\", fuzzy = 0.8 }", "fee-schedule"),
            t("fee schedule")
        );
    }

    #[test]
    fn casts() {
        assert_eq!(run("f64", "", " 12 "), Value::F64(12.0));
        assert_eq!(run("f64", "", "1e3"), Value::F64(1000.0));
        assert_eq!(run("f64", "", "-0"), Value::F64(-0.0));
        assert_eq!(run("f64", "", "abc"), Value::Null);
        assert_eq!(run("f64", "", "inf"), Value::Null);
        assert_eq!(run("i64", "", " 12 "), Value::I64(12));
        assert_eq!(run("i64", "", "1.5"), Value::Null);
        assert_eq!(run("bool", "", "Yes"), Value::Bool(true));
        assert_eq!(run("bool", "", "0"), Value::Bool(false));
        assert_eq!(run("bool", "", "maybe"), Value::Null);
        assert_eq!(run("date", "", "1970-01-01"), Value::Date(0));
        assert_eq!(run("date", "", "2026-01-01"), Value::Date(20454));
        assert_eq!(run("date", "", "2/29/2024"), Value::Date(19782));
        assert_eq!(run("date", "", "2026-02-30"), Value::Null);
        assert_eq!(run("date", "", "13/1/2026"), Value::Null);
        assert_eq!(run("date", "", "2026-1-01"), Value::Null);
        assert_eq!(run("date", "", "2026-01/01"), Value::Null);
        assert_eq!(run("date", "", "01-02/2026"), Value::Null);
        assert_eq!(run("json", "", "a\"b"), t("\"a\\\"b\""));
        assert_eq!(run("list", "", "a | b"), Value::List(vec!["a".into(), "b".into()]));
        let empty = column("from = \"payer.name\"\ntype = \"f64\"");
        assert_eq!(empty.value(&record("1"), &Metadata::default()), Value::Null);
    }

    #[test]
    fn codes() {
        let value = |kind: &str, transform: &str| {
            let c = column(&format!(
                "from = \"charge.codes\"\ntype = \"{kind}\"\ntransform = [{transform}]"
            ));
            c.value(&record("1"), &Metadata::default())
        };
        assert_eq!(
            value("json", "\"codes_by_type\""),
            t(r#"{"":["X1"],"CPT":["99213"],"RC":["0450"]}"#)
        );
        assert_eq!(
            value("list", "\"codes_list\""),
            Value::List(vec!["CPT:99213".into(), "RC:0450".into(), "X1".into()])
        );
        assert_eq!(
            value("json", ""),
            t(
                r#"[{"code":"99213","type":"cpt"},{"code":"0450","type":"RC"},{"code":"X1","type":null}]"#
            )
        );
        let joined = column(
            "from = [\"charge.modifiers\", \"payer.name\", \"payer.dollar\"]\ntype = \"string\"",
        );
        assert_eq!(joined.value(&record("5"), &Metadata::default()), t("50|LT||5"));
        let position = column("from = \"record.position\"\ntype = \"i64\"");
        assert_eq!(position.value(&record("5"), &Metadata::default()), Value::I64(7));
    }

    #[test]
    fn hash() {
        // The FNV spec's test vectors for "" and "a".
        assert_eq!(fnv1a_128(""), "6c62272e07bb014262b821756295c58d");
        assert_eq!(fnv1a_128("a"), "d228cb696f1a8caf78912b704e4a8964");
        assert_eq!(run("string", "\"hash\"", "a"), t("d228cb696f1a8caf78912b704e4a8964"));
    }

    #[test]
    fn load_errors() {
        let message = |toml: &str| match Config::parse(toml) {
            Err(Error::Config { message }) => message,
            other => panic!("{other:?}"),
        };
        let col = |body: &str| message(&format!("[[column]]\nname = \"c\"\n{body}"));
        assert!(message("[output]\ncompresion = \"zstd\"").contains("unknown field `compresion`"));
        assert!(message("[output]\ncompresion = \"zstd\"").contains("line 2"));
        assert_eq!(
            col("from = \"payer.nam\"\ntype = \"string\""),
            "column `c`: unknown field `payer.nam`"
        );
        let twice = "[[column]]\nname = \"c\"\nfrom = \"payer.name\"\ntype = \"string\"\n";
        assert_eq!(message(&twice.repeat(2)), "column `c` is defined twice");
        assert_eq!(
            col("from = \"payer.name\"\nplugin = \"p.f\"\ntype = \"string\""),
            "column `c`: needs one of `from` and `plugin`"
        );
        assert_eq!(col("type = \"string\""), "column `c`: needs one of `from` and `plugin`");
        assert_eq!(
            col("plugin = \"p.f\"\ntype = \"string\""),
            "column `c`: plugin `p` is not declared in a [[plugin]] table"
        );
        let bad = |transform: &str| {
            col(&format!("from = \"payer.name\"\ntype = \"string\"\ntransform = [{transform}]"))
        };
        assert_eq!(bad("{ rond = 2 }"), "column `c`: unknown transform or argument `rond = 2`");
        assert_eq!(bad("\"trimm\""), "column `c`: unknown transform `trimm`");
        assert_eq!(
            bad("{ round = 2, map = {} }"),
            "column `c`: a transform table has one key: `{ map = {}, round = 2 }`"
        );
        assert_eq!(bad("{ match = \"settings\" }"), "column `c`: unknown domain `settings`");
        assert_eq!(
            bad("{ match = \"setting\", fuzzy = 2 }"),
            "column `c`: `fuzzy` is a similarity from 0 to 1"
        );
        assert!(col("from = \"payer.name\"\ntype = \"text\"").contains("unknown variant `text`"));
        assert_eq!(
            message("[[filter]]\nplugin = \"q.keep\""),
            "filter: plugin `q` is not declared in a [[plugin]] table"
        );
    }

    #[test]
    fn builtin() {
        let config = Config::builtin();
        let sources: Vec<String> = (config.columns.iter())
            .map(|c| match &c.source {
                Source::Fields(f) => format!("{:?}", f[0]),
                Source::Plugin(p) => p.clone(),
            })
            .collect();
        let expected: Vec<&str> = Field::names()
            .filter(|n| !n.starts_with("metadata.") && !n.starts_with("record."))
            .collect();
        assert_eq!(sources, expected);
        for (c, source) in config.columns.iter().zip(&sources) {
            assert_eq!(c.name, source.replace('.', "_"));
        }
    }
}
