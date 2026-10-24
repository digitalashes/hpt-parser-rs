use super::{Transform, Type, Value, Work};
use crate::Code;
use crate::text::{is_space, split, trim};
use std::borrow::Cow;
use std::collections::BTreeMap;

/// A value as one text: lists joined by `|`, codes as `codes_list` gives them.
pub(super) fn flat(work: Work<'_>) -> Option<Cow<'_, str>> {
    match work {
        Work::Null => None,
        Work::Text(s) => Some(s),
        Work::Json(s) => Some(Cow::Owned(s)),
        Work::List(list) => Some(Cow::Owned(list.join("|"))),
        Work::Codes(codes) => Some(Cow::Owned(codes_list(codes).join("|"))),
    }
}

pub(super) fn apply<'a>(t: &Transform, work: Work<'a>) -> Work<'a> {
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
            (pairs.iter().find(|(from, ..)| from == key))
                .or_else(|| {
                    let key = key.to_lowercase();
                    pairs.iter().find(|(_, lower, _)| *lower == key)
                })
                .map_or(s, |(.., to)| to.clone())
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
pub(super) fn similarity(a: &str, b: &str) -> f64 {
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
pub(super) fn fnv1a_128(s: &str) -> String {
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
    to_json(&codes.iter().map(CodeJson::from).collect::<Vec<_>>())
}

#[derive(serde::Serialize)]
pub(super) struct CodeJson<'a> {
    code: &'a str,
    #[serde(rename = "type")]
    code_type: Option<&'a str>,
}

impl<'a> From<&'a Code> for CodeJson<'a> {
    fn from(c: &'a Code) -> Self {
        CodeJson { code: &c.code, code_type: c.code_type.as_deref() }
    }
}

pub(super) fn cast(work: Work<'_>, kind: Type) -> Value<'_> {
    let s = match (work, kind) {
        (Work::Json(s), Type::Json) => return Value::Text(s.into()),
        (Work::Codes(codes), Type::Json) => return Value::Text(codes_json(codes).into()),
        (Work::Codes(codes), kind) => return cast(Work::List(codes_list(codes)), kind),
        (Work::List(list), Type::List) => return Value::List(list),
        (Work::List(list), Type::Json) => return Value::Text(to_json(&list).into()),
        (Work::List(list), Type::String) => return Value::Text(list.join("|").into()),
        (Work::Null | Work::List(_), _) => return Value::Null,
        (Work::Text(s), _) => s,
        (Work::Json(s), _) => Cow::Owned(s),
    };
    let t = trim(&s);
    let value = match kind {
        Type::String => Some(Value::Text(s)),
        Type::F64 => t.parse::<f64>().ok().filter(|n| n.is_finite()).map(Value::F64),
        Type::I64 => t.parse().ok().map(Value::I64),
        Type::Bool => match t.to_lowercase().as_str() {
            "true" | "yes" | "y" | "1" => Some(Value::Bool(true)),
            "false" | "no" | "n" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        Type::Date => date(t).map(Value::Date),
        Type::Json => Some(Value::Text(to_json(&s).into())),
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
