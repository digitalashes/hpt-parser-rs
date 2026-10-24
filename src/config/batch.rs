use super::transform::CodeJson;
use super::{Column, Config, FIELDS, Field, Source, Transform, Value, Work};
use super::{apply, cast, read};
use crate::{Error, Metadata, Record};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::value::RawValue;
use std::borrow::Cow;

impl Column {
    /// Whether a plugin computes the column or one of its transforms.
    pub(crate) fn has_plugins(&self) -> bool {
        matches!(self.source, Source::Plugin(_))
            || self.transforms.iter().any(|t| matches!(t, Transform::Plugin(_)))
    }

    /// Whether the column reads only `charge.*` and `metadata.*` fields, so that its value
    /// is the same for every record of one charge read with the same metadata (with a
    /// plugin transform the values come made for the whole batch).
    pub(crate) fn per_charge(&self) -> bool {
        let Source::Fields(fields) = &self.source else {
            return false;
        };
        let shared =
            |f: &Field| ["charge.", "metadata."].iter().any(|p| FIELDS[f.0].0.starts_with(p));
        fields.iter().all(shared)
    }

    /// The column's values for a batch of records, owned, to go to another thread; each
    /// plugin function is called once for the batch, the other transforms once per
    /// record.
    pub(crate) fn values<'a>(
        &self,
        records: &'a [Record],
        metadata: &'a Metadata,
        call: &mut Call<'_>,
    ) -> Result<Vec<Value<'static>>, Error> {
        let mut works: Vec<Work<'a>> = match &self.source {
            Source::Fields(fields) => records.iter().map(|r| read(fields, r, metadata)).collect(),
            Source::Plugin(h) => {
                let input = records_json(records, &h.fields, metadata);
                plugin_values(&h.plugin, &call(&h.plugin, &input)?, records.len())?
            }
        };
        for t in &self.transforms {
            works = match t {
                Transform::Plugin(f) => {
                    let input = serde_json::to_vec(&Values { values: &works })
                        .expect("text, lists and codes always serialize");
                    plugin_values(f, &call(f, &input)?, works.len())?
                }
                t => works.into_iter().map(|w| apply(t, w)).collect(),
            };
        }
        Ok(works.into_iter().map(|w| cast(w, self.kind).into_owned()).collect())
    }
}

impl Value<'_> {
    fn into_owned(self) -> Value<'static> {
        match self {
            Value::Null => Value::Null,
            Value::Text(s) => Value::Text(Cow::Owned(s.into_owned())),
            Value::List(items) => Value::List(items),
            Value::F64(x) => Value::F64(x),
            Value::I64(x) => Value::I64(x),
            Value::Bool(x) => Value::Bool(x),
            Value::Date(x) => Value::Date(x),
        }
    }
}

/// Calls a plugin function, `<plugin>.<function>`, with its JSON input.
pub(crate) type Call<'c> = dyn FnMut(&str, &[u8]) -> Result<Box<RawValue>, Error> + Send + 'c;

impl Config {
    /// Drops the records a `[[filter]]` does not keep, one filter after another.
    pub(crate) fn filter(
        &self,
        records: &mut Vec<Record>,
        metadata: &Metadata,
        call: &mut Call<'_>,
    ) -> Result<(), Error> {
        for f in &self.filters {
            if records.is_empty() {
                break;
            }
            let input = records_json(records, &f.fields, metadata);
            let answer = call(&f.plugin, &input)?;
            let keep = serde_json::from_str(answer.get()).ok().map(|k: Keep<_>| k.keep);
            let keep = (items(&f.plugin, keep, "keep", records.len())?.iter())
                .map(|k| match k.get() {
                    "true" => Ok(true),
                    "false" => Ok(false),
                    k => {
                        Err(plugin_error(&f.plugin, format!("`keep` holds {k}, not true or false")))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut keep = keep.into_iter();
            records.retain(|_| keep.next().expect("one answer per record"));
        }
        Ok(())
    }
}

pub(crate) fn plugin_error(function: &str, message: String) -> Error {
    Error::Plugin { plugin: function.into(), message }
}

/// `{"values":[..]}`: a value transform's input, the answer of it and of a plugin column.
#[derive(serde::Serialize, serde::Deserialize)]
struct Values<T> {
    values: T,
}

/// `{"keep":[..]}`: a filter's answer.
#[derive(serde::Deserialize)]
struct Keep<T> {
    keep: T,
}

/// The answer's `key` array, if it has one, with an item per input.
fn items<'a>(
    function: &str,
    items: Option<Vec<&'a RawValue>>,
    key: &str,
    n: usize,
) -> Result<Vec<&'a RawValue>, Error> {
    let items =
        items.ok_or_else(|| plugin_error(function, format!("the answer has no `{key}` array")))?;
    if items.len() != n {
        let message = format!("{} items in `{key}` for {n} inputs", items.len());
        return Err(plugin_error(function, message));
    }
    Ok(items)
}

fn plugin_values(function: &str, answer: &RawValue, n: usize) -> Result<Vec<Work<'static>>, Error> {
    let values = serde_json::from_str(answer.get()).ok().map(|v: Values<_>| v.values);
    Ok(items(function, values, "values", n)?.into_iter().map(json_work).collect())
}

/// `{"records":[..]}`: each record flat, keyed by the names of `fields`, absent values
/// left out.
pub(super) fn records_json(records: &[Record], fields: &[Field], metadata: &Metadata) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Records<'a> {
        records: Vec<Flat<'a>>,
    }
    let records = records.iter().map(|record| Flat { record, fields, metadata }).collect();
    serde_json::to_vec(&Records { records }).expect("text, lists and codes always serialize")
}

struct Flat<'a> {
    record: &'a Record,
    fields: &'a [Field],
    metadata: &'a Metadata,
}

impl Serialize for Flat<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        for field in self.fields {
            let work = field.get(self.record, self.metadata);
            if !matches!(work, Work::Null) {
                map.serialize_entry(field, &work)?;
            }
        }
        map.end()
    }
}

/// The field's name.
impl Serialize for Field {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(FIELDS[self.0].0)
    }
}

/// Text as a string, lists as arrays, codes as `[{"code":..,"type":..}]`.
impl Serialize for Work<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Work::Null => s.serialize_unit(),
            Work::Text(text) => s.serialize_str(text),
            Work::List(list) => list.serialize(s),
            Work::Codes(codes) => s.collect_seq(codes.iter().map(CodeJson::from)),
            Work::Json(text) => match serde_json::from_str::<&RawValue>(text) {
                Ok(json) => json.serialize(s),
                Err(_) => s.serialize_str(text),
            },
        }
    }
}

/// A plugin's value: a string as text, an array of strings as a list, numbers and booleans
/// as their text, other JSON as its text as the plugin wrote it.
fn json_work(value: &RawValue) -> Work<'static> {
    let text = value.get();
    match text.as_bytes().first() {
        Some(b'n') => Work::Null,
        Some(b'"') => serde_json::from_str(text).map_or(Work::Null, |s| Work::Text(Cow::Owned(s))),
        Some(b'[') => {
            serde_json::from_str(text).map_or_else(|_| Work::Json(text.into()), Work::List)
        }
        Some(b'{') => Work::Json(text.into()),
        _ => Work::Text(Cow::Owned(text.into())),
    }
}
