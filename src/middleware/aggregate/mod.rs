//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Keeps one state document per key in this process and updates it with every message.

use crate::middleware::raw_json::RawPairs;
use crate::models::{AggregateConsistency, AggregateEmit, AggregateMiddleware, AggregateOnError};
use crate::support::interpolation::CompiledTemplate;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::Context;
use async_trait::async_trait;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::{Map, Value};
use states::States;
use std::any::Any;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use store::{StateStore, StateWrite};
use writer::Writer;
use zen_expression::compiler::{FetchFastTarget, Opcode};
use zen_expression::variable::{Symbol, VariableMap};
use zen_expression::vm::VM;
use zen_expression::{compile_expression, expression::Standard, Expression, Variable};

mod states;
pub(crate) mod store;
mod writer;

const STATE: &str = "state";
const META: &str = "meta";

/// Metadata naming the entries, by `into`, a message left out under `on_error: skip`.
pub const SKIPPED_KEY: &str = "mqb.aggregate.skipped";
/// Metadata naming the entries, by `into`, whose state had already seen a later `time`.
pub const LATE_KEY: &str = "mqb.aggregate.late";
/// Metadata that, set to `true` on a message, folds it as the newest even when it is late.
pub const OVERWRITE_KEY: &str = "mqb.aggregate.overwrite";

type Folded = Result<CanonicalMessage, (CanonicalMessage, PublisherError)>;
/// One entry's result for a message: entry index, key, new state, JSON to write, late.
type Update = (usize, String, Stored, Vec<u8>, bool);
/// Per message, the JSON each entry writes and whether it came late; `None` until that
/// entry ran.
type Emitted = anyhow::Result<Vec<Option<(Vec<u8>, bool)>>>;

/// The entries a message left out and the ones it reached late.
#[derive(Default)]
struct Notes {
    skipped: Vec<usize>,
    late: Vec<usize>,
}

/// Longest key of one state; with the `into` prefix it fits the store's key column.
const MAX_KEY_LEN: usize = 384;
/// Rounds of reload-and-refold before a contended batch is given back to the source.
const MAX_ROUNDS: usize = 32;
/// States one entry keeps in memory unless `max_keys` says otherwise.
const DEFAULT_MAX_KEYS: usize = 1_000_000;

struct Entry {
    key: CompiledTemplate,
    /// Set when `key` is one top-level payload field, which the parsed payload answers.
    key_field: Option<String>,
    fold: Fold,
    into: Vec<String>,
    /// `,"<into>":` when `into` is one top-level field, to append the result to a payload.
    label: Option<Vec<u8>>,
    emit: AggregateEmit,
}

/// How an entry computes its next state.
enum Fold {
    Zen {
        expression: Expression<Standard>,
        output: Option<Expression<Standard>>,
    },
    /// Built-in aggregates over `f64`; `width` is the number of state slots and `clock`
    /// the slot holding the newest time seen, in seconds.
    Fields {
        fields: Vec<Field>,
        width: usize,
        clock: Option<usize>,
    },
}

/// What one entry is configured with.
struct EntryConfig<'a> {
    key: &'a str,
    expression: Option<&'a str>,
    fields: Option<&'a BTreeMap<String, String>>,
    output: Option<&'a str>,
    into: &'a str,
    emit: AggregateEmit,
    /// The middleware reads a `time` from the message.
    timed: bool,
}

/// How much the weight of earlier values shrinks with a new one.
#[derive(Clone, Copy)]
enum Decay {
    /// To `1 - alpha` with every message.
    Count(f64),
    /// To half over this many seconds.
    HalfLife(f64),
}

enum Op {
    Count,
    Sum,
    Min,
    Max,
    Last,
    Mean,
    Ema(Decay),
    /// Sample variance, or its root, of values weighted by `decay`; 1 weights all equally.
    Spread {
        decay: Decay,
        root: bool,
    },
    /// Count, or sum, whose earlier values fade to half over this many seconds.
    Decayed {
        half_life: f64,
        sum: bool,
    },
}

/// When a value comes, relative to the newest time its state has seen.
#[derive(Clone, Copy)]
enum Step {
    /// This many seconds after it.
    After(f64),
    /// This many seconds before it.
    Late(f64),
}

/// One built-in aggregate: reads the value at `source` and owns the state slots from `at`.
struct Field {
    /// `"<name>":`, as written into the result.
    label: Vec<u8>,
    op: Op,
    /// Index of the value it reads; unused by `count`.
    source: usize,
    at: usize,
}

impl Field {
    /// Parses `count`, `sum(path)`, `min`, `max`, `last`, `mean`, `stddev`, `variance`,
    /// `ema(path, alpha)`, `ema_stddev`, `ema_variance`, where a duration such as `5m` in
    /// place of `alpha` is a half-life, or `decayed_count(half_life)`, `decayed_sum(path, half_life)`.
    fn new(
        name: &str,
        spec: &str,
        at: usize,
        sources: &mut Vec<Vec<String>>,
    ) -> anyhow::Result<Self> {
        let invalid = || {
            anyhow::anyhow!(
                "aggregate: invalid field '{name}: {spec}'; a field is `count`, `decayed_count(half-life)` or one of sum, min, max, last, mean, stddev, variance (path), ema, ema_stddev, ema_variance (path, alpha or half-life), decayed_sum (path, half-life)"
            )
        };
        let mut label = serde_json::to_vec(name)?;
        label.push(b':');
        let spec = spec.trim();
        if spec == "count" {
            return Ok(Self {
                label,
                op: Op::Count,
                source: 0,
                at,
            });
        }
        let (function, args) = spec
            .strip_suffix(')')
            .and_then(|s| s.split_once('('))
            .ok_or_else(invalid)?;
        let half_life = |arg: &str| {
            if arg.parse::<f64>().is_ok() {
                anyhow::bail!(
                    "aggregate: field '{name}' takes a half-life such as `1h`, not an alpha"
                );
            }
            seconds(arg).ok_or_else(invalid)
        };
        if function.trim() == "decayed_count" {
            return Ok(Self {
                label,
                op: Op::Decayed {
                    half_life: half_life(args.trim())?,
                    sum: false,
                },
                source: 0,
                at,
            });
        }
        let mut args = args.split(',').map(str::trim);
        let path: Vec<String> = args
            .next()
            .unwrap_or_default()
            .split('.')
            .map(str::to_string)
            .collect();
        if path.iter().any(String::is_empty) {
            return Err(invalid());
        }
        let mut decay = || {
            let arg = args.next().ok_or_else(invalid)?;
            let Ok(alpha) = arg.parse::<f64>() else {
                return seconds(arg).map(Decay::HalfLife).ok_or_else(invalid);
            };
            if !(alpha > 0.0 && alpha <= 1.0) {
                anyhow::bail!("aggregate: field '{name}': alpha must be in (0, 1]");
            }
            Ok(Decay::Count(1.0 - alpha))
        };
        let spread = |decay, root| Op::Spread { decay, root };
        let op = match function.trim() {
            "sum" => Op::Sum,
            "min" => Op::Min,
            "max" => Op::Max,
            "last" => Op::Last,
            "mean" => Op::Mean,
            "stddev" => spread(Decay::Count(1.0), true),
            "variance" => spread(Decay::Count(1.0), false),
            "ema" => Op::Ema(decay()?),
            "ema_stddev" => spread(decay()?, true),
            "ema_variance" => spread(decay()?, false),
            "decayed_sum" => Op::Decayed {
                half_life: half_life(args.next().ok_or_else(invalid)?)?,
                sum: true,
            },
            _ => return Err(invalid()),
        };
        if args.next().is_some() {
            return Err(invalid());
        }
        let source = match sources.iter().position(|s| *s == path) {
            Some(source) => source,
            None => {
                sources.push(path);
                sources.len() - 1
            }
        };
        Ok(Self {
            label,
            op,
            source,
            at,
        })
    }

    fn width(&self) -> usize {
        match self.op {
            Op::Mean | Op::Ema(_) => 2,
            Op::Spread { .. } => 4,
            _ => 1,
        }
    }

    /// Whether it decays by time and so needs the state's clock.
    fn timed(&self) -> bool {
        match self.op {
            Op::Ema(decay) | Op::Spread { decay, .. } => matches!(decay, Decay::HalfLife(_)),
            Op::Decayed { .. } => true,
            _ => false,
        }
    }

    /// Folds `x` into the slots at `step`; `first` on a zeroed state. A late value weighs
    /// `0.5^(lateness / half_life)`, so the result does not depend on the order of arrival.
    /// A result that is no longer finite is not kept.
    fn apply(&self, slots: &mut [f64], x: f64, first: bool, step: Step) {
        let a = self.at;
        let keep = |slot: &mut f64, x: f64| {
            let next = *slot + x;
            if next.is_finite() {
                *slot = next;
            }
        };
        // How much earlier values fade, and what the new one weighs.
        let by_time = |half_life: f64| match step {
            Step::After(elapsed) => (0.5f64.powf(elapsed / half_life), 1.0),
            Step::Late(early) => (1.0, 0.5f64.powf(early / half_life)),
        };
        let weigh = |decay: Decay| match decay {
            Decay::HalfLife(seconds) => by_time(seconds),
            Decay::Count(decay) => (decay, 1.0),
        };
        match self.op {
            Op::Count => slots[a] += 1.0,
            Op::Sum => keep(&mut slots[a], x),
            Op::Min => slots[a] = if first { x } else { slots[a].min(x) },
            Op::Max => slots[a] = if first { x } else { slots[a].max(x) },
            Op::Last if matches!(step, Step::Late(_)) => {}
            Op::Last => slots[a] = x,
            Op::Mean => {
                keep(&mut slots[a], x);
                slots[a + 1] += 1.0;
            }
            // Weighted sum and weight: the mean of the values seen, without a start bias.
            Op::Ema(decay) => {
                let (fade, w) = weigh(decay);
                slots[a] = slots[a] * fade + x * w;
                slots[a + 1] = slots[a + 1] * fade + w;
            }
            // Weight, mean, weighted squared deviations, sum of squared weights (West 1979).
            Op::Spread { decay, .. } => {
                let (fade, w) = weigh(decay);
                let weight = slots[a] * fade + w;
                let mean = slots[a + 1] + (x - slots[a + 1]) * w / weight;
                let squares = slots[a + 2] * fade + w * (x - slots[a + 1]) * (x - mean);
                if squares.is_finite() {
                    slots[a] = weight;
                    slots[a + 1] = mean;
                    slots[a + 2] = squares;
                    slots[a + 3] = slots[a + 3] * fade * fade + w * w;
                }
            }
            Op::Decayed { half_life, sum } => {
                let (fade, w) = by_time(half_life);
                slots[a] *= fade;
                keep(&mut slots[a], if sum { x * w } else { w });
            }
        }
    }

    /// Writes the result, `since` seconds after the state's newest time.
    fn write(&self, slots: &[f64], since: f64, out: &mut Vec<u8>) -> anyhow::Result<()> {
        let a = self.at;
        out.extend_from_slice(&self.label);
        match self.op {
            Op::Decayed { half_life, .. } => {
                serde_json::to_writer(out, &(slots[a] * 0.5f64.powf(since / half_life)))?
            }
            Op::Count => write!(out, "{}", slots[a] as u64)?,
            Op::Mean | Op::Ema(_) => serde_json::to_writer(out, &(slots[a] / slots[a + 1]))?,
            Op::Spread { root, .. } => {
                // Unbiased for weighted values; `n - 1` when all weigh the same.
                let freedom = slots[a] - slots[a + 3] / slots[a];
                if freedom.is_nan() || freedom <= 1e-9 {
                    out.extend_from_slice(b"null");
                    return Ok(());
                }
                let variance = (slots[a + 2] / freedom).max(0.0);
                serde_json::to_writer(out, &if root { variance.sqrt() } else { variance })?
            }
            _ => serde_json::to_writer(out, &slots[a])?,
        }
        Ok(())
    }
}

/// Folds the values of one message, at time `now`, into the slots of a `fields` entry.
/// `true` when the state had already seen a later time: the message then weighs less, and
/// `last` keeps its value. With `overwrite` it is folded as the newest and sets the clock.
fn fold_slots(
    fields: &[Field],
    clock: Option<usize>,
    slots: &mut [f64],
    values: &[f64],
    now: f64,
    first: bool,
    overwrite: bool,
) -> bool {
    let elapsed = match clock {
        Some(at) if !first => now - slots[at],
        _ => 0.0,
    };
    let late = elapsed < 0.0 && !overwrite;
    let step = match late {
        true => Step::Late(-elapsed),
        false => Step::After(elapsed.max(0.0)),
    };
    for field in fields {
        let x = values.get(field.source).copied().unwrap_or(0.0);
        field.apply(slots, x, first, step);
    }
    if let Some(at) = clock {
        slots[at] = if first || overwrite {
            now
        } else {
            slots[at].max(now)
        };
    }
    late
}

/// Seconds from the state's newest time to `now`; 0 without a clock or for a late `now`.
fn since(clock: Option<usize>, slots: &[f64], now: f64) -> f64 {
    clock.map_or(0.0, |at| (now - slots[at]).max(0.0))
}

/// Whether the message asks to be folded as the newest, see [`OVERWRITE_KEY`].
fn overwrites(msg: &CanonicalMessage) -> bool {
    msg.metadata
        .get(OVERWRITE_KEY)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// Seconds of a duration such as `500ms`, `30s`, `5m`, `2h` or `7d`.
fn seconds(text: &str) -> Option<f64> {
    let (number, unit) = text.split_at(text.find(|c: char| c.is_ascii_alphabetic())?);
    let scale = match unit {
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        _ => return None,
    };
    let seconds = number.trim().parse::<f64>().ok()? * scale;
    (seconds.is_finite() && seconds > 0.0).then_some(seconds)
}

fn number(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|x| x.is_finite())
}

/// Seconds since the epoch of an epoch number, taken as milliseconds above 1e11, or of an
/// RFC 3339 time; one without a zone is UTC.
fn timestamp(text: &str) -> Option<f64> {
    if let Some(epoch) = number(text) {
        return Some(if epoch.abs() > 1e11 {
            epoch / 1000.0
        } else {
            epoch
        });
    }
    let text = text.trim();
    let bytes = text.as_bytes();
    let part = |at: usize, len: usize| -> Option<i64> {
        let digits = text.get(at..at + len)?;
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse().ok())?
    };
    let (year, month, day) = (part(0, 4)?, part(5, 2)?, part(8, 2)?);
    let (hour, minute, second) = (part(11, 2)?, part(14, 2)?, part(17, 2)?);
    let separated = bytes[4] == b'-'
        && bytes[7] == b'-'
        && matches!(bytes[10], b'T' | b't' | b' ')
        && bytes[13] == b':'
        && bytes[16] == b':';
    if !separated || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let rest = text.get(19..)?;
    let zone = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let fraction = match &rest[..zone] {
        "" => 0.0,
        fraction => number(fraction).filter(|_| fraction.starts_with('.'))?,
    };
    let offset = match &rest[zone..] {
        "" | "Z" | "z" => 0,
        signed if signed.len() == 6 && signed.as_bytes()[3] == b':' => {
            let sign = if signed.starts_with('-') { -1 } else { 1 };
            let at = 19 + zone;
            sign * (part(at + 1, 2)? * 3600 + part(at + 4, 2)? * 60)
        }
        _ => return None,
    };
    // Days since the epoch of a civil date (Hinnant).
    let year = if month <= 2 { year - 1 } else { year };
    let (era, of_era) = (year.div_euclid(400), year.rem_euclid(400));
    let of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + of_era * 365 + of_era / 4 - of_era / 100 + of_year - 719_468;
    let whole = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(whole as f64 + fraction)
}

/// Writes the result object of a `fields` entry.
fn write_fields(
    fields: &[Field],
    slots: &[f64],
    since: f64,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    out.push(b'{');
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        field.write(slots, since, out)?;
    }
    out.push(b'}');
    Ok(())
}

/// A payload whose top-level fields are read without building a JSON tree.
enum Doc<'a> {
    Raw(Vec<(&'a str, &'a serde_json::value::RawValue)>),
    /// Keys with escapes cannot be borrowed; those payloads take the full parse.
    Tree(Value),
}

impl<'a> Doc<'a> {
    fn parse(payload: &'a [u8]) -> anyhow::Result<Self> {
        if let Ok(RawPairs(pairs)) = serde_json::from_slice::<RawPairs>(payload) {
            return Ok(Self::Raw(pairs));
        }
        match serde_json::from_slice(payload).context("aggregate: payload is not JSON")? {
            tree @ Value::Object(_) => Ok(Self::Tree(tree)),
            _ => anyhow::bail!("aggregate: payload must be a JSON object"),
        }
    }

    /// The number at `path`; numeric strings count, as a CSV source delivers them.
    fn number(&self, path: &[String]) -> Option<f64> {
        self.value(path, number)
    }

    /// The time at `path` in seconds since the epoch.
    fn time(&self, path: &[String]) -> Option<f64> {
        self.value(path, timestamp)
    }

    /// The number or string at `path`, read by `text`.
    fn value(&self, path: &[String], text: fn(&str) -> Option<f64>) -> Option<f64> {
        match self {
            Self::Raw(pairs) => {
                let (top, rest) = path.split_first()?;
                let mut raw = pairs.iter().rev().find(|(key, _)| key == top)?.1;
                for name in rest {
                    let RawPairs(inner) = serde_json::from_str(raw.get()).ok()?;
                    raw = inner.iter().rev().find(|(key, _)| key == name)?.1;
                }
                let raw = raw.get();
                let unquoted = raw.strip_prefix('"').and_then(|t| t.strip_suffix('"'));
                text(unquoted.unwrap_or(raw))
            }
            Self::Tree(tree) => match path.iter().try_fold(tree, |at, name| at.get(name))? {
                Value::Number(n) => text(&n.to_string()),
                Value::String(s) => text(s),
                _ => None,
            },
        }
    }

    /// The text of the top-level `field`, when it is a plain string, number or boolean.
    fn text(&self, field: &str) -> Option<&'a str> {
        let Self::Raw(pairs) = self else { return None };
        let raw = pairs.iter().rev().find(|(key, _)| *key == field)?.1.get();
        match raw.as_bytes().first()? {
            b'"' => Some(&raw[1..raw.len() - 1]).filter(|inner| !inner.contains('\\')),
            b'{' | b'[' | b'n' => None,
            _ => Some(raw),
        }
    }
}

impl Entry {
    fn new(config: EntryConfig<'_>, sources: &mut Vec<Vec<String>>) -> anyhow::Result<Self> {
        let EntryConfig {
            key,
            expression,
            fields,
            output,
            into,
            emit,
            timed,
        } = config;
        let fold = match (expression, fields) {
            (Some(expression), None) => Fold::Zen {
                expression: compile(expression, "expression")?,
                output: output.map(|o| compile(o, "output")).transpose()?,
            },
            (None, Some(specs)) if output.is_none() && !specs.is_empty() => {
                let mut fields = Vec::with_capacity(specs.len());
                let mut width = 0;
                for (name, spec) in specs {
                    let field = Field::new(name, spec, width, sources)?;
                    width += field.width();
                    fields.push(field);
                }
                let clock = (timed || fields.iter().any(Field::timed)).then_some(width);
                Fold::Fields {
                    fields,
                    width: width + usize::from(clock.is_some()),
                    clock,
                }
            }
            (None, Some(_)) => {
                anyhow::bail!("aggregate: `fields` must not be empty and takes no `output`")
            }
            _ => anyhow::bail!("aggregate: set either `expression` or `fields`"),
        };
        let into: Vec<String> = into
            .trim()
            .trim_start_matches('$')
            .trim_start_matches('.')
            .split('.')
            .map(str::to_string)
            .collect();
        if into.iter().any(String::is_empty) {
            anyhow::bail!("aggregate: invalid `into` path '{}'", into.join("."));
        }
        let key = CompiledTemplate::compile(key, None)?;
        if !key.is_dynamic() {
            anyhow::bail!("aggregate: `key` must read the message, e.g. `${{payload:sensor_id}}`");
        }
        let key_field = key
            .sole_payload_path()
            .filter(|path| !path.is_empty() && !path.contains('.'))
            .map(str::to_string);
        let label = match into.as_slice() {
            [top] => {
                let mut label = vec![b','];
                serde_json::to_writer(&mut label, top)?;
                label.push(b':');
                Some(label)
            }
            _ => None,
        };
        Ok(Self {
            key,
            key_field,
            fold,
            into,
            label,
            emit,
        })
    }

    /// Like `key`, for a payload that was not parsed into a tree.
    fn raw_key<'a>(&self, msg: &CanonicalMessage, doc: &Doc<'a>) -> Option<Cow<'a, str>> {
        let fast = self.key_field.as_deref().and_then(|field| doc.text(field));
        let rendered = || String::from_utf8(self.key.render_resolved(Some(msg))?).ok();
        fast.map(Cow::Borrowed)
            .or_else(|| rendered().map(Cow::Owned))
            .filter(|key| !key.is_empty())
    }

    /// Reads a state of this entry from its JSON text in the store.
    fn parse_state(&self, json: &str) -> anyhow::Result<Stored> {
        let Fold::Fields { width, clock, .. } = &self.fold else {
            return Stored::parse(json);
        };
        let mut slots: Vec<f64> = serde_json::from_str(json)
            .context("aggregate: a stored `fields` state is not a list of numbers")?;
        // A state stored before `time` was set has no clock yet.
        if clock.is_some() && slots.len() + 1 == *width {
            slots.push(0.0);
        }
        if slots.len() != *width {
            anyhow::bail!("aggregate: a stored state does not match the configured `fields`");
        }
        Ok(Stored::Floats(slots.into()))
    }

    /// Whether every value this entry reads is there; a missing one is NaN.
    fn ready(&self, values: &[f64], now: f64) -> bool {
        let Fold::Fields { fields, clock, .. } = &self.fold else {
            return true;
        };
        let read = |field: &Field| {
            matches!(field.op, Op::Count | Op::Decayed { sum: false, .. })
                || !values[field.source].is_nan()
        };
        fields.iter().all(read) && !(clock.is_some() && now.is_nan())
    }

    /// The next state and the JSON to emit for a `fields` entry, and whether it came late.
    fn fold_fields(
        &self,
        previous: Option<&Stored>,
        values: &[f64],
        now: f64,
        overwrite: bool,
    ) -> anyhow::Result<(Stored, Vec<u8>, bool)> {
        let Fold::Fields {
            fields,
            width,
            clock,
        } = &self.fold
        else {
            unreachable!("only called for a `fields` entry")
        };
        let mut json = Vec::with_capacity(96);
        let (mut slots, first) = match previous {
            Some(Stored::Floats(slots)) => (slots.clone(), false),
            _ => (vec![0.0; *width].into_boxed_slice(), true),
        };
        match (self.emit, first) {
            (AggregateEmit::Previous, true) => json.extend_from_slice(b"null"),
            (AggregateEmit::Previous, false) => {
                write_fields(fields, &slots, since(*clock, &slots, now), &mut json)?
            }
            (AggregateEmit::Updated, _) => {}
        }
        let late = fold_slots(fields, *clock, &mut slots, values, now, first, overwrite);
        if self.emit == AggregateEmit::Updated {
            write_fields(fields, &slots, 0.0, &mut json)?;
        }
        Ok((Stored::Floats(slots), json, late))
    }

    /// The state key for this message; `None` when it is missing or empty.
    fn key(&self, msg: &CanonicalMessage, fields: &VariableMap) -> Option<String> {
        let fast = self.key_field.as_deref().and_then(|field| {
            Some(match fields.get_str(field)? {
                Variable::String(s) => s.as_str().to_string(),
                Variable::Number(n) => n.normalize().to_string(),
                Variable::Bool(b) => b.to_string(),
                _ => return None,
            })
        });
        fast.or_else(|| String::from_utf8(self.key.render_resolved(Some(msg))?).ok())
            .filter(|key| !key.is_empty())
    }

    /// The key of a state in the store: entries of one middleware share a table.
    fn store_key(&self, key: &str) -> String {
        format!("{}:{key}", self.into.join("."))
    }

    fn reads_meta(&self) -> bool {
        match &self.fold {
            Fold::Zen { expression, output } => {
                reads(expression, META) || output.as_ref().is_some_and(|o| reads(o, META))
            }
            Fold::Fields { .. } => false,
        }
    }
}

fn compile(expression: &str, field: &str) -> anyhow::Result<Expression<Standard>> {
    compile_expression(expression).map_err(|e| anyhow::anyhow!("aggregate: invalid `{field}`: {e}"))
}

/// Whether the expression reads the top-level name `root`.
fn reads(expression: &Expression<Standard>, root: &str) -> bool {
    expression.bytecode().iter().any(|opcode| match opcode {
        Opcode::FetchEnv(name) => name.as_ref() == root,
        Opcode::FetchFast(targets) => {
            targets.iter().find_map(|target| match target {
                FetchFastTarget::String(name) => Some(name.as_ref()),
                _ => None,
            }) == Some(root)
        }
        _ => false,
    })
}

/// The entries of one `aggregate` middleware and their states, one map per entry.
struct Aggregate {
    entries: Vec<Entry>,
    reads_meta: bool,
    /// Payload paths the `fields` entries read, each once per message.
    sources: Vec<Vec<String>>,
    /// Payload path of the message's time; the clock when unset.
    time: Option<Vec<String>>,
    /// An entry keeps a clock, so every message needs a time.
    clocked: bool,
    on_error: AggregateOnError,
    /// States are read from the store and never changed.
    read_only: bool,
    /// The first skipped entry and the first late message were logged.
    noted: [AtomicBool; 2],
    /// No entry is an expression: the payload is never parsed into a tree.
    plain: bool,
    /// Plain, and every result is one distinct top-level field appended to the payload.
    append: bool,
    states: Mutex<Vec<States>>,
    /// States one entry keeps in memory; 0 is unlimited.
    max_keys: usize,
    /// The first forgotten state was logged.
    warned: AtomicBool,
    /// With a store the states live there and are loaded per batch.
    store: Option<Arc<dyn StateStore>>,
    /// One batch at a time against the store, so this instance does not race itself.
    gate: tokio::sync::Mutex<()>,
    /// Set for `consistency: single_writer`; it then owns the states.
    writer: Option<Arc<Writer>>,
}

impl Drop for Aggregate {
    fn drop(&mut self) {
        if let Some(writer) = &self.writer {
            writer.close();
        }
    }
}

/// Resolves once the states a batch changed are stored; at once without `single_writer`.
struct Flush(Option<(Arc<Writer>, u64)>);

impl Flush {
    async fn wait(self) -> anyhow::Result<()> {
        match self.0 {
            Some((writer, seq)) => writer.flushed(seq).await,
            None => Ok(()),
        }
    }
}

/// Buffers reused across the messages of a batch.
#[derive(Default)]
struct Scratch {
    values: Vec<f64>,
}

impl Aggregate {
    fn new(config: &AggregateMiddleware) -> anyhow::Result<Self> {
        let mut entries = Vec::with_capacity(config.entries.len() + 1);
        let mut sources = Vec::new();
        let folds = config.expression.is_some() || config.fields.is_some();
        match (&config.key, &config.into) {
            (Some(key), Some(into)) if folds => entries.push(Entry::new(
                EntryConfig {
                    key,
                    expression: config.expression.as_deref(),
                    fields: config.fields.as_ref(),
                    output: config.output.as_deref(),
                    into,
                    emit: config.emit,
                    timed: config.time.is_some(),
                },
                &mut sources,
            )?),
            (None, None) if !folds && config.output.is_none() => {}
            _ => anyhow::bail!(
                "aggregate: `key`, `into` and `expression` or `fields` must be set together"
            ),
        }
        for e in &config.entries {
            entries.push(Entry::new(
                EntryConfig {
                    key: &e.key,
                    expression: e.expression.as_deref(),
                    fields: e.fields.as_ref(),
                    output: e.output.as_deref(),
                    into: &e.into,
                    emit: e.emit,
                    timed: config.time.is_some(),
                },
                &mut sources,
            )?);
        }
        if entries.is_empty() {
            return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                "aggregate: set `key`, `expression` and `into`, or list `entries`"
            ))
            .into());
        }
        // `into` names an entry's states in the store, so two entries cannot share one.
        let paths: HashSet<&[String]> = entries.iter().map(|e| e.into.as_slice()).collect();
        if paths.len() != entries.len() {
            anyhow::bail!("aggregate: two entries write to the same `into`");
        }
        let plain = (entries.iter()).all(|e| matches!(e.fold, Fold::Fields { .. }));
        let clocked = |e: &Entry| matches!(e.fold, Fold::Fields { clock: Some(_), .. });
        let time: Option<Vec<String>> = (config.time.as_deref())
            .map(|path| path.trim().split('.').map(str::to_string).collect());
        if time
            .as_ref()
            .is_some_and(|path| path.iter().any(String::is_empty))
            || (time.is_some() && !entries.iter().any(clocked))
        {
            return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                "aggregate: `time` is a payload path and needs an entry with `fields`"
            ))
            .into());
        }
        let max_keys = config.max_keys.unwrap_or(DEFAULT_MAX_KEYS);
        let tops: HashSet<&str> = entries.iter().map(|e| e.into[0].as_str()).collect();
        Ok(Self {
            reads_meta: entries.iter().any(Entry::reads_meta),
            append: plain
                && tops.len() == entries.len()
                && entries.iter().all(|e| e.label.is_some()),
            plain,
            sources,
            time,
            clocked: entries.iter().any(clocked),
            on_error: config.on_error,
            read_only: config.read_only,
            noted: Default::default(),
            states: Mutex::new(entries.iter().map(|_| States::new(max_keys)).collect()),
            max_keys,
            warned: AtomicBool::new(false),
            entries,
            store: None,
            gate: tokio::sync::Mutex::new(()),
            writer: None,
        })
    }

    /// Like `new`, and opens the configured `store`.
    async fn connect(config: &AggregateMiddleware, route_name: &str) -> anyhow::Result<Self> {
        // Whatever `new` rejects is a config that cannot work, so the route stops.
        let mut aggregate = Self::new(config).map_err(|e| {
            if e.is::<crate::errors::InvalidConfig>() {
                e
            } else {
                crate::errors::InvalidConfig(e).into()
            }
        })?;
        if config.read_only && config.store.is_none() {
            return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                "aggregate: `read_only` needs a `store` to read the states from"
            ))
            .into());
        }
        if config.store.is_none() && config.consistency != AggregateConsistency::SingleWriter {
            return Err(crate::errors::InvalidConfig(anyhow::anyhow!(
                "aggregate: without a `store` the states live in this process only and are \
                 lost on restart; set a `store`, or `consistency: single_writer` to accept that"
            ))
            .into());
        }
        if let Some(spec) = &config.store {
            let longest = aggregate.entries.iter().map(|e| e.store_key("").len());
            if longest.max().unwrap_or(0) + MAX_KEY_LEN > store::MAX_KEY_LEN {
                anyhow::bail!("aggregate: an `into` path is too long to key a stored state");
            }
            let store = store::build_store(spec, route_name).await?;
            // Read-only states are loaded per batch: another instance changes them.
            match config.consistency {
                AggregateConsistency::SingleWriter if !config.read_only => {
                    aggregate.write_behind(store)
                }
                _ => aggregate.store = Some(store),
            }
        }
        Ok(aggregate)
    }

    /// Keeps the states in memory and writes them to `store` behind the batches.
    fn write_behind(&mut self, store: Arc<dyn StateStore>) {
        let prefixes = self.entries.iter().map(|e| e.into.join(".")).collect();
        self.writer = Some(Writer::start(store, prefixes, self.max_keys));
    }

    /// Folds a batch. The outer error is a store failure: nothing was folded, retry the batch.
    async fn fold_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Vec<Folded>, Flush)> {
        if let Some(writer) = &self.writer {
            let (folded, seq) = self.fold_behind(writer, messages).await?;
            return Ok((folded, Flush(Some((writer.clone(), seq)))));
        }
        let folded = match &self.store {
            Some(store) => self.fold_shared(store.as_ref(), messages).await?,
            None => self.fold(messages),
        };
        Ok((folded, Flush(None)))
    }

    /// Loads the states the batch needs and memory lacks, then folds in memory. Returns the
    /// batch number its commit waits on.
    async fn fold_behind(
        &self,
        writer: &Writer,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<(Vec<Folded>, u64)> {
        let _gate = writer.gate.lock().await;
        let n = self.entries.len();
        let mut missing = vec![HashSet::new(); n];
        {
            let inner = writer.lock();
            for keys in messages.iter().map(|m| self.keys(m)) {
                for (i, key) in keys.into_iter().enumerate() {
                    if let Some(key) = key.filter(|key| !inner.states[i].contains_key(key)) {
                        missing[i].insert(key);
                    }
                }
            }
        }
        let ids: Vec<String> = (missing.iter().zip(&self.entries))
            .flat_map(|(keys, entry)| keys.iter().map(|key| entry.store_key(key)))
            .collect();
        let mut loaded = match ids.is_empty() {
            true => HashMap::new(),
            false => writer.store.load_many(&ids).await?,
        };
        let mut inner = writer.lock();
        for (i, keys) in missing.into_iter().enumerate() {
            let entry = &self.entries[i];
            for key in keys {
                if let Some((json, version)) = loaded.remove(&entry.store_key(&key)) {
                    inner.states[i].insert(key.clone(), entry.parse_state(&json)?);
                    inner.versions[i].insert(key, version);
                }
            }
        }
        let writer::Inner { states, dirty, .. } = &mut *inner;
        let folded = self.fold_into(messages, states, Some(dirty));
        Ok((folded, writer.folded(&mut inner)))
    }

    /// Loads the states of the batch, folds, and writes them back guarded by their versions.
    /// Keys another instance changed in between are reloaded and only they are folded again.
    async fn fold_shared(
        &self,
        store: &dyn StateStore,
        messages: Vec<CanonicalMessage>,
    ) -> anyhow::Result<Vec<Folded>> {
        let _gate = self.gate.lock().await;
        let n = self.entries.len();
        let keys: Vec<Vec<Option<String>>> = messages.iter().map(|m| self.keys(m)).collect();
        let mut pending = vec![HashSet::new(); n];
        for keys in &keys {
            for (i, key) in keys.iter().enumerate() {
                pending[i].extend(key.clone());
            }
        }
        let mut states: Vec<States> = (0..n).map(|_| States::new(0)).collect();
        let mut versions: Vec<HashMap<String, i64>> = vec![HashMap::new(); n];
        let mut slots: Vec<Emitted> = messages.iter().map(|_| Ok(vec![None; n])).collect();
        let mut settled = false;
        for round in 0..MAX_ROUNDS {
            let ids: Vec<String> = (pending.iter().zip(&self.entries))
                .flat_map(|(keys, entry)| keys.iter().map(|key| entry.store_key(key)))
                .collect();
            let mut loaded = store.load_many(&ids).await?;
            for (i, keys) in pending.iter().enumerate() {
                for key in keys {
                    let version = match loaded.remove(&self.entries[i].store_key(key)) {
                        Some((json, version)) => {
                            states[i].insert(key.clone(), self.entries[i].parse_state(&json)?);
                            version
                        }
                        None => {
                            states[i].remove(key);
                            0
                        }
                    };
                    versions[i].insert(key.clone(), version);
                }
            }
            let only = (round > 0).then_some(pending.as_slice());
            let touched = self.fold_round(&messages, &keys, &mut states, only, &mut slots);
            let mut writes = Vec::new();
            let mut owners = Vec::new();
            for (i, keys) in touched.into_iter().enumerate() {
                for key in keys {
                    let mut state = Vec::new();
                    let folded = states[i]
                        .get(&key)
                        .context("aggregate: a folded state is gone")?;
                    folded.write_json(&mut state)?;
                    writes.push(StateWrite {
                        key: self.entries[i].store_key(&key),
                        state: String::from_utf8(state)?,
                        expected: versions[i][&key],
                    });
                    owners.push((i, key));
                }
            }
            let lost = match writes.is_empty() {
                true => Vec::new(),
                false => store.store_many(&writes).await?,
            };
            if lost.is_empty() {
                settled = true;
                break;
            }
            pending = vec![HashSet::new(); n];
            for at in lost {
                let (i, key) = owners
                    .get(at)
                    .context("aggregate: the store reported a write it was not given")?;
                pending[*i].insert(key.clone());
            }
        }
        if !settled {
            anyhow::bail!("aggregate: states kept changing in the store for {MAX_ROUNDS} rounds");
        }
        Ok(messages
            .into_iter()
            .zip(slots)
            .map(|(msg, slot)| {
                let outcome = slot.and_then(|emitted| self.payload(&msg, emitted));
                self.settle(msg, outcome)
            })
            .collect())
    }

    /// The key of every entry for this message; `None` where it cannot be read.
    fn keys(&self, msg: &CanonicalMessage) -> Vec<Option<String>> {
        let usable = |key: &String| key.len() <= MAX_KEY_LEN;
        let none = || vec![None; self.entries.len()];
        if self.plain {
            let Ok(doc) = Doc::parse(&msg.payload) else {
                return none();
            };
            return (self.entries.iter())
                .map(|e| e.raw_key(msg, &doc).map(Cow::into_owned).filter(usable))
                .collect();
        }
        let Ok(doc) = serde_json::from_slice::<Variable>(&msg.payload) else {
            return none();
        };
        let Some(object) = doc.as_object() else {
            return none();
        };
        let fields = object.borrow();
        (self.entries.iter())
            .map(|e| e.key(msg, &fields).filter(usable))
            .collect()
    }

    /// Puts the outcome of folding into the message. Under `on_error: skip` a message that
    /// cannot be folded goes on unchanged.
    fn settle(
        &self,
        mut msg: CanonicalMessage,
        outcome: anyhow::Result<(Vec<u8>, Notes)>,
    ) -> Folded {
        let notes = match outcome {
            Ok((payload, notes)) => {
                msg.payload = payload.into();
                notes
            }
            Err(e) if self.on_error == AggregateOnError::Skip => {
                tracing::debug!("aggregate: passing a message on unchanged: {e:#}");
                Notes {
                    skipped: (0..self.entries.len()).collect(),
                    late: Vec::new(),
                }
            }
            Err(e) => return Err((msg, PublisherError::NonRetryable(e))),
        };
        let marks = [
            (SKIPPED_KEY, notes.skipped, "left out by `on_error: skip`"),
            (
                LATE_KEY,
                notes.late,
                "older than the `time` its state had seen",
            ),
        ];
        for ((key, entries, what), noted) in marks.into_iter().zip(&self.noted) {
            if entries.is_empty() {
                continue;
            }
            let names: Vec<String> = (entries.iter())
                .map(|i| self.entries[*i].into.join("."))
                .collect();
            let names = names.join(",");
            if !noted.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "aggregate: entry '{names}' was {what}; such messages carry the \
                     metadata `{key}`. Logged once."
                );
            }
            msg.metadata.insert(key.to_string(), names);
        }
        Ok(msg)
    }

    /// Folds the messages that still have a key in `only` (all when `None`) and returns the
    /// keys whose state changed. A message that fails keeps its error and is skipped later.
    fn fold_round(
        &self,
        messages: &[CanonicalMessage],
        keys: &[Vec<Option<String>>],
        states: &mut [States],
        only: Option<&[HashSet<String>]>,
        slots: &mut [Emitted],
    ) -> Vec<HashSet<String>> {
        let mut vm = VM::new();
        let mut touched = vec![HashSet::new(); self.entries.len()];
        for ((msg, keys), slot) in messages.iter().zip(keys).zip(slots) {
            let affected = match only {
                Some(only) => (keys.iter().zip(only))
                    .any(|(key, set)| key.as_ref().is_some_and(|key| set.contains(key))),
                None => true,
            };
            let Ok(emitted) = slot else { continue };
            if !affected {
                continue;
            }
            match self.compute(msg, states, &mut vm, only) {
                Ok(updates) => {
                    let mut changed = Vec::with_capacity(updates.len());
                    for (i, key, next, json, late) in updates {
                        emitted[i] = Some((json, late));
                        changed.push((i, key, next));
                    }
                    // The states are stored before the payload is written, so an output
                    // that has no place in the payload fails here and changes no state.
                    if let Err(e) = self.payload(msg, emitted.clone()) {
                        *slot = Err(e);
                        continue;
                    }
                    if !self.read_only {
                        for (i, key, next) in changed {
                            touched[i].insert(key.clone());
                            states[i].insert(key, next);
                        }
                    }
                }
                Err(e) => *slot = Err(e),
            }
        }
        touched
    }

    /// Writes what the entries emitted into the message's payload. An entry that emitted
    /// nothing was skipped.
    fn payload(
        &self,
        msg: &CanonicalMessage,
        emitted: Vec<Option<(Vec<u8>, bool)>>,
    ) -> anyhow::Result<(Vec<u8>, Notes)> {
        let mut notes = Notes::default();
        let mut values = Vec::with_capacity(emitted.len());
        for (i, (entry, emitted)) in self.entries.iter().zip(emitted).enumerate() {
            let Some((json, late)) = emitted else {
                notes.skipped.push(i);
                continue;
            };
            if late {
                notes.late.push(i);
            }
            values.push((entry.into.as_slice(), json));
        }
        if values.is_empty() {
            return Ok((msg.payload.to_vec(), notes));
        }
        Ok((write_payload(&msg.payload, values)?, notes))
    }

    /// Updates the states message by message, in order. A failing message changes no state.
    fn fold(&self, messages: Vec<CanonicalMessage>) -> Vec<Folded> {
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let folded = self.fold_into(messages, &mut states, None);
        // Without a store a dropped state is gone, so say so once.
        let dropped: usize = states.iter_mut().map(|s| s.turn(|_| false)).sum();
        if dropped > 0 && !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "aggregate: more than `max_keys` ({}) states; the least recently used are \
                 forgotten and start again when their key returns",
                self.max_keys
            );
        }
        folded
    }

    /// Folds into `states`, noting every key it changed in `dirty`.
    fn fold_into(
        &self,
        messages: Vec<CanonicalMessage>,
        states: &mut [States],
        mut dirty: Option<&mut Vec<HashSet<String>>>,
    ) -> Vec<Folded> {
        let mut vm = VM::new();
        let mut scratch = Scratch::default();
        messages
            .into_iter()
            .map(|msg| {
                let dirty = dirty.as_deref_mut();
                let outcome = self.update(&msg, states, &mut vm, &mut scratch, dirty);
                self.settle(msg, outcome)
            })
            .collect()
    }

    /// Runs every entry for one message and returns its new payload.
    fn update(
        &self,
        msg: &CanonicalMessage,
        states: &mut [States],
        vm: &mut VM,
        scratch: &mut Scratch,
        mut dirty: Option<&mut Vec<HashSet<String>>>,
    ) -> anyhow::Result<(Vec<u8>, Notes)> {
        if self.append {
            let dirty = dirty.as_deref_mut();
            if let Some(done) = self.update_appending(msg, states, scratch, dirty)? {
                return Ok(done);
            }
        }
        let mut updates = self.compute(msg, states, vm, None)?;
        let mut emitted = vec![None; self.entries.len()];
        for update in &mut updates {
            emitted[update.0] = Some((std::mem::take(&mut update.3), update.4));
        }
        let done = self.payload(msg, emitted)?;
        for (i, key, next, ..) in updates {
            if let Some(dirty) = dirty.as_deref_mut() {
                dirty[i].insert(key.clone());
            }
            states[i].insert(key, next);
        }
        Ok(done)
    }

    /// Reads every source path into `values` and returns the message's time in seconds.
    /// A value that is missing or not a number fails, or is NaN under `on_error: skip`.
    fn read_values(&self, doc: &Doc<'_>, values: &mut Vec<f64>) -> anyhow::Result<f64> {
        let lenient = self.on_error == AggregateOnError::Skip;
        values.clear();
        for path in &self.sources {
            let value = match doc.number(path) {
                Some(value) => value,
                None if lenient => f64::NAN,
                None => anyhow::bail!(
                    "aggregate: field '{}' is missing or not a number",
                    path.join(".")
                ),
            };
            values.push(value);
        }
        let Some(path) = &self.time else {
            let clock = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
            return Ok(match self.clocked {
                true => clock().map_or(0.0, |since| since.as_secs_f64()),
                false => 0.0,
            });
        };
        match doc.time(path) {
            Some(time) => Ok(time),
            None if lenient => Ok(f64::NAN),
            None => anyhow::bail!(
                "aggregate: `time` field '{}' is missing or not a time",
                path.join(".")
            ),
        }
    }

    /// The fast path of `fields`: reads only what it needs, updates the states in place and
    /// appends the results to the payload bytes. `None` when the results cannot be appended.
    fn update_appending(
        &self,
        msg: &CanonicalMessage,
        states: &mut [States],
        scratch: &mut Scratch,
        mut dirty: Option<&mut Vec<HashSet<String>>>,
    ) -> anyhow::Result<Option<(Vec<u8>, Notes)>> {
        let doc = Doc::parse(&msg.payload)?;
        let Doc::Raw(pairs) = &doc else {
            return Ok(None);
        };
        let end = msg.payload.iter().rposition(|b| !b.is_ascii_whitespace());
        let taken = |entry: &Entry| pairs.iter().any(|(key, _)| *key == entry.into[0]);
        let Some(end) = end.filter(|_| !pairs.is_empty() && !self.entries.iter().any(taken)) else {
            return Ok(None);
        };
        let now = self.read_values(&doc, &mut scratch.values)?;
        let lenient = self.on_error == AggregateOnError::Skip;
        let overwrite = overwrites(msg);
        let mut notes = Notes::default();
        let mut keys = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            let key = entry.raw_key(msg, &doc);
            let whole = key.as_ref().is_some_and(|key| key.len() <= MAX_KEY_LEN)
                && entry.ready(&scratch.values, now);
            // An entry to skip takes the path that can leave one out.
            if lenient && !whole {
                return Ok(None);
            }
            let key = key.context("aggregate: `key` has no value for this message")?;
            if key.len() > MAX_KEY_LEN {
                anyhow::bail!("aggregate: `key` is longer than {MAX_KEY_LEN} bytes");
            }
            keys.push(key);
        }

        let mut out = Vec::with_capacity(msg.payload.len() + 96 * self.entries.len());
        out.extend_from_slice(&msg.payload[..end]);
        for (i, (entry, key)) in self.entries.iter().zip(keys).enumerate() {
            let (
                Fold::Fields {
                    fields,
                    width,
                    clock,
                },
                Some(label),
            ) = (&entry.fold, &entry.label)
            else {
                unreachable!("`append` is set for top-level `fields` entries only")
            };
            if let Some(dirty) = dirty.as_deref_mut() {
                if !dirty[i].contains(key.as_ref()) {
                    dirty[i].insert(key.to_string());
                }
            }
            let (state, mut first) = states[i].slot(key.as_ref());
            if !matches!(state, Stored::Floats(slots) if slots.len() == *width) {
                *state = Stored::Floats(vec![0.0; *width].into());
                first = true;
            }
            let Stored::Floats(slots) = state else {
                unreachable!("the state was just made a float state")
            };
            out.extend_from_slice(label);
            match (entry.emit, first) {
                (AggregateEmit::Previous, true) => out.extend_from_slice(b"null"),
                (AggregateEmit::Previous, false) => {
                    write_fields(fields, slots, since(*clock, slots, now), &mut out)?
                }
                (AggregateEmit::Updated, _) => {}
            }
            if fold_slots(
                fields,
                *clock,
                slots,
                &scratch.values,
                now,
                first,
                overwrite,
            ) {
                notes.late.push(i);
            }
            if entry.emit == AggregateEmit::Updated {
                write_fields(fields, slots, 0.0, &mut out)?;
            }
        }
        out.push(b'}');
        Ok(Some((out, notes)))
    }

    /// Evaluates the entries of one message whose key is in `only` (every entry when
    /// `None`) against `states`, without changing them.
    fn compute(
        &self,
        msg: &CanonicalMessage,
        states: &[States],
        vm: &mut VM,
        only: Option<&[HashSet<String>]>,
    ) -> anyhow::Result<Vec<Update>> {
        let mut values = Vec::new();
        let mut now = 0.0;
        let raw = match self.plain || !self.sources.is_empty() || self.time.is_some() {
            true => Some(Doc::parse(&msg.payload)?),
            false => None,
        };
        if let Some(raw) = &raw {
            now = self.read_values(raw, &mut values)?;
        }
        // Under `on_error: skip` an entry that cannot be computed is left out.
        let skip = |entry: &Entry, e: anyhow::Error| match self.on_error {
            AggregateOnError::Skip => {
                tracing::debug!("aggregate: skipping '{}': {e:#}", entry.into.join("."));
                Ok(())
            }
            _ => Err(e),
        };
        let tree = match self.plain {
            true => None,
            false => Some(self.tree(msg)?),
        };
        let state = Symbol::from(STATE);
        let mut evaluate = |expression: &Expression<Standard>, with: Variable| {
            let doc = tree.as_ref().expect("an expression entry has a tree");
            if let Some(object) = doc.as_object() {
                object.borrow_mut().insert(state.clone(), with);
            }
            expression
                .evaluate_with(doc.clone(), vm)
                .map_err(|e| anyhow::anyhow!("aggregate: expression failed: {e}"))
        };

        let mut updates = Vec::with_capacity(self.entries.len());
        for (i, (entry, states)) in self.entries.iter().zip(states.iter()).enumerate() {
            let key = match (tree.as_ref().and_then(Variable::as_object), &raw) {
                (Some(object), _) => entry.key(msg, &object.borrow()),
                (None, Some(raw)) => entry.raw_key(msg, raw).map(Cow::into_owned),
                (None, None) => None,
            };
            let key = match key {
                Some(key) if key.len() <= MAX_KEY_LEN => key,
                Some(_) => {
                    let long = "aggregate: `key` is longer than";
                    skip(entry, anyhow::anyhow!("{long} {MAX_KEY_LEN} bytes"))?;
                    continue;
                }
                None => {
                    let missing = "aggregate: `key` has no value for this message";
                    skip(entry, anyhow::anyhow!(missing))?;
                    continue;
                }
            };
            if only.is_some_and(|only| !only[i].contains(&key)) {
                continue;
            }
            let previous = states.get(&key);
            let Fold::Zen { expression, output } = &entry.fold else {
                if !entry.ready(&values, now) {
                    let missing = "aggregate: a field it reads is missing or not a number";
                    skip(entry, anyhow::anyhow!(missing))?;
                    continue;
                }
                let (next, json, late) =
                    entry.fold_fields(previous, &values, now, overwrites(msg))?;
                updates.push((i, key, next, json, late));
                continue;
            };
            let before = previous.map_or(Variable::Null, Stored::to_variable);
            let next = match evaluate(expression, before.clone()) {
                Ok(next) => next,
                Err(e) => {
                    skip(entry, e)?;
                    continue;
                }
            };
            let shown = match (entry.emit, previous) {
                (AggregateEmit::Updated, _) => next.clone(),
                (AggregateEmit::Previous, Some(_)) => before,
                (AggregateEmit::Previous, None) => Variable::Null,
            };
            let shown = match output {
                Some(output) if !matches!(shown, Variable::Null) => match evaluate(output, shown) {
                    Ok(shown) => shown,
                    Err(e) => {
                        skip(entry, e)?;
                        continue;
                    }
                },
                _ => shown,
            };
            let mut json = Vec::with_capacity(96);
            write_json(&shown, &mut json)?;
            updates.push((i, key, Stored::from_variable(&next), json, false));
        }
        Ok(updates)
    }

    /// The payload as the document expressions read, with `meta` when one of them uses it.
    fn tree(&self, msg: &CanonicalMessage) -> anyhow::Result<Variable> {
        let doc: Variable =
            serde_json::from_slice(&msg.payload).context("aggregate: payload is not JSON")?;
        let Some(object) = doc.as_object() else {
            anyhow::bail!("aggregate: payload must be a JSON object");
        };
        if self.reads_meta {
            let meta = msg
                .metadata
                .iter()
                .map(|(k, v)| {
                    (
                        Symbol::from(k.as_str()),
                        Variable::String(v.as_str().into()),
                    )
                })
                .collect();
            object
                .borrow_mut()
                .insert(Symbol::from(META), Variable::from_object(meta));
        }
        Ok(doc)
    }
}

/// A state document that can cross threads; `Variable` is `Rc`-based. Numbers stay
/// `Decimal`, so a state is not rounded through `f64` between two messages.
enum Stored {
    Null,
    Bool(bool),
    Number(Decimal),
    String(Box<str>),
    Array(Vec<Stored>),
    Object(Vec<(Box<str>, Stored)>),
    /// The slots of a `fields` entry.
    Floats(Box<[f64]>),
}

impl Stored {
    fn from_variable(variable: &Variable) -> Self {
        match variable {
            Variable::Null => Self::Null,
            Variable::Bool(b) => Self::Bool(*b),
            Variable::Number(n) => Self::Number(*n),
            Variable::String(s) => Self::String(s.as_str().into()),
            Variable::Array(items) => {
                Self::Array(items.borrow().iter().map(Self::from_variable).collect())
            }
            Variable::Object(fields) => Self::Object(
                fields
                    .borrow()
                    .iter()
                    .map(|(k, v)| (k.as_str().into(), Self::from_variable(v)))
                    .collect(),
            ),
            dynamic => Self::from_variable(&Variable::from(Value::from(dynamic.clone()))),
        }
    }

    fn to_variable(&self) -> Variable {
        match self {
            Self::Null => Variable::Null,
            Self::Bool(b) => Variable::Bool(*b),
            Self::Number(n) => Variable::Number(*n),
            Self::String(s) => Variable::String(Symbol::from(s.as_ref())),
            Self::Array(items) => {
                Variable::from_array(items.iter().map(Self::to_variable).collect())
            }
            Self::Object(fields) => Variable::from_object(
                fields
                    .iter()
                    .map(|(k, v)| (Symbol::from(k.as_ref()), v.to_variable()))
                    .collect(),
            ),
            // An expression never reads a `fields` state.
            Self::Floats(_) => Variable::Null,
        }
    }

    /// JSON text for the store. Numbers are written as exact decimals.
    fn write_json(&self, out: &mut Vec<u8>) -> anyhow::Result<()> {
        match self {
            Self::Null => out.extend_from_slice(b"null"),
            Self::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            Self::Number(n) => write!(out, "{n}")?,
            Self::String(s) => serde_json::to_writer(out, s.as_ref())?,
            Self::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.write_json(out)?;
                }
                out.push(b']');
            }
            Self::Object(fields) => {
                out.push(b'{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    serde_json::to_writer(&mut *out, key.as_ref())?;
                    out.push(b':');
                    value.write_json(out)?;
                }
                out.push(b'}');
            }
            Self::Floats(slots) => serde_json::to_writer(out, slots)?,
        }
        Ok(())
    }

    /// Reads what `write_json` wrote, keeping every digit of a number.
    fn parse(json: &str) -> anyhow::Result<Self> {
        use serde_json::value::RawValue;
        let json = json.trim();
        Ok(match json.as_bytes().first() {
            Some(b'{') => match serde_json::from_str::<RawPairs>(json) {
                Ok(RawPairs(pairs)) => Self::Object(
                    pairs
                        .into_iter()
                        .map(|(k, v)| Ok((k.into(), Self::parse(v.get())?)))
                        .collect::<anyhow::Result<_>>()?,
                ),
                // Keys with escapes cannot be borrowed; numbers then pass through `f64`.
                Err(_) => {
                    Self::from_variable(&Variable::from(serde_json::from_str::<Value>(json)?))
                }
            },
            Some(b'[') => Self::Array(
                serde_json::from_str::<Vec<&RawValue>>(json)?
                    .into_iter()
                    .map(|item| Self::parse(item.get()))
                    .collect::<anyhow::Result<_>>()?,
            ),
            Some(b'"') => Self::String(serde_json::from_str::<String>(json)?.into()),
            Some(b't') | Some(b'f') => Self::Bool(serde_json::from_str(json)?),
            Some(b'n') => Self::Null,
            _ => Self::Number(
                Decimal::from_str(json)
                    .or_else(|_| Decimal::from_scientific(json))
                    .with_context(|| {
                        format!("aggregate: stored state has an invalid number '{json}'")
                    })?,
            ),
        })
    }
}

/// Serializes a `Variable`. Its own `Serialize` truncates fractions, and the detour
/// through `Value` parses every number back from text.
fn write_json(variable: &Variable, out: &mut Vec<u8>) -> anyhow::Result<()> {
    match variable {
        Variable::Null => out.extend_from_slice(b"null"),
        Variable::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        // Integers exactly; fractions as the shortest `f64`, like `Value::from(Variable)`.
        Variable::Number(n) => match n.is_integer().then(|| n.to_i128()).flatten() {
            Some(integer) => write!(out, "{integer}")?,
            None => serde_json::to_writer(out, &n.as_f64())?,
        },
        Variable::String(s) => serde_json::to_writer(out, s.as_str())?,
        Variable::Array(items) => {
            out.push(b'[');
            for (i, item) in items.borrow().iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json(item, out)?;
            }
            out.push(b']');
        }
        Variable::Object(fields) => {
            out.push(b'{');
            for (i, (key, value)) in fields.borrow().iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key.as_str())?;
                out.push(b':');
                write_json(value, out)?;
            }
            out.push(b'}');
        }
        dynamic => serde_json::to_writer(out, &Value::from(dynamic.clone()))?,
    }
    Ok(())
}

/// JSON being assembled: text copied as is, or an object whose fields are set by path.
enum Node {
    Raw(Vec<u8>),
    Object(Vec<(String, Node)>),
}

impl Node {
    /// Sets `json` at `path`. `Ok(false)` when an object on the way has escaped keys.
    fn insert(&mut self, path: &[String], json: Vec<u8>) -> anyhow::Result<bool> {
        let Some((key, rest)) = path.split_first() else {
            *self = Node::Raw(json);
            return Ok(true);
        };
        if let Node::Raw(raw) = self {
            if raw.first() != Some(&b'{') {
                anyhow::bail!("aggregate: cannot set '{key}': its parent is not an object");
            }
            let Ok(RawPairs(pairs)) = serde_json::from_slice::<RawPairs>(raw) else {
                return Ok(false);
            };
            let fields = pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), Node::Raw(v.get().as_bytes().to_vec())))
                .collect();
            *self = Node::Object(fields);
        }
        let Node::Object(fields) = self else {
            unreachable!("raw text was just expanded")
        };
        let at = match fields.iter().position(|(k, _)| k == key) {
            Some(at) => at,
            None => {
                fields.push((key.clone(), Node::Object(Vec::new())));
                fields.len() - 1
            }
        };
        fields[at].1.insert(rest, json)
    }

    fn write(&self, out: &mut Vec<u8>) -> anyhow::Result<()> {
        match self {
            Node::Raw(raw) => out.extend_from_slice(raw),
            Node::Object(fields) => {
                out.push(b'{');
                for (i, (key, node)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    serde_json::to_writer(&mut *out, key)?;
                    out.push(b':');
                    node.write(out)?;
                }
                out.push(b'}');
            }
        }
        Ok(())
    }
}

/// Writes each JSON value at its path, copying every untouched field verbatim.
fn write_payload(payload: &[u8], values: Vec<(&[String], Vec<u8>)>) -> anyhow::Result<Vec<u8>> {
    if let Some(spliced) = splice(payload, &values)? {
        return Ok(spliced);
    }
    // Keys with escapes cannot be borrowed; those payloads take the full parse.
    let mut doc: Value = serde_json::from_slice(payload)?;
    for (path, json) in values {
        insert_at(&mut doc, path, serde_json::from_slice(&json)?)?;
    }
    Ok(serde_json::to_vec(&doc)?)
}

fn splice(payload: &[u8], values: &[(&[String], Vec<u8>)]) -> anyhow::Result<Option<Vec<u8>>> {
    let Ok(RawPairs(pairs)) = serde_json::from_slice::<RawPairs>(payload) else {
        return Ok(None);
    };
    let mut written: Vec<(&str, Node)> = Vec::with_capacity(values.len());
    for (path, json) in values {
        let (top, rest) = path.split_first().expect("path is non-empty");
        let at = match written.iter().position(|(key, _)| key == top) {
            Some(at) => at,
            None => {
                let node = match pairs.iter().rev().find(|(key, _)| key == top) {
                    Some((_, raw)) if !rest.is_empty() => Node::Raw(raw.get().as_bytes().to_vec()),
                    _ => Node::Object(Vec::new()),
                };
                written.push((top, node));
                written.len() - 1
            }
        };
        if !written[at].1.insert(rest, json.clone())? {
            return Ok(None);
        }
    }

    let mut out = Vec::with_capacity(payload.len() + 128 * written.len());
    out.push(b'{');
    for (key, raw) in &pairs {
        if written.iter().any(|(top, _)| top == key) {
            continue;
        }
        out.push(b'"');
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(b"\":");
        out.extend_from_slice(raw.get().as_bytes());
        out.push(b',');
    }
    for (key, node) in &written {
        serde_json::to_writer(&mut out, key)?;
        out.push(b':');
        node.write(&mut out)?;
        out.push(b',');
    }
    out.pop();
    out.push(b'}');
    Ok(Some(out))
}

/// Writes `value` at the dotted `path`, creating objects along the way.
fn insert_at(root: &mut Value, path: &[String], value: Value) -> anyhow::Result<()> {
    let (last, parents) = path.split_last().expect("path is non-empty");
    let mut cur = root;
    for key in parents {
        let Value::Object(map) = cur else {
            anyhow::bail!("aggregate: cannot nest under '{key}': it is not an object");
        };
        cur = map
            .entry(key.as_str())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let Value::Object(map) = cur else {
        anyhow::bail!("aggregate: cannot set '{last}': its parent is not an object");
    };
    map.insert(last.clone(), value);
    Ok(())
}

pub struct AggregatePublisher {
    inner: Box<dyn MessagePublisher>,
    aggregate: Aggregate,
}

impl AggregatePublisher {
    pub async fn new(
        inner: Box<dyn MessagePublisher>,
        config: &AggregateMiddleware,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            aggregate: Aggregate::connect(config, route_name).await?,
        })
    }

    async fn fold(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<(Vec<Folded>, Flush), PublisherError> {
        let folded = self.aggregate.fold_batch(messages).await;
        folded.map_err(PublisherError::Retryable)
    }
}

#[async_trait]
impl MessagePublisher for AggregatePublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        let (mut folded, flush) = self.fold(vec![message]).await?;
        let message = folded
            .pop()
            .expect("one result per message")
            .map_err(|(_, e)| e)?;
        let sent = self.inner.send(message).await?;
        flush.wait().await.map_err(PublisherError::Retryable)?;
        Ok(sent)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let mut folded = Vec::with_capacity(messages.len());
        let mut failed = Vec::new();
        let (results, flush) = self.fold(messages).await?;
        for result in results {
            match result {
                Ok(m) => folded.push(m),
                Err(f) => failed.push(f),
            }
        }
        if folded.is_empty() {
            return Ok(SentBatch::Partial {
                responses: None,
                failed,
            });
        }
        let sent = self.inner.send_batch(folded).await?;
        // The batch is not done before the states it changed are stored.
        flush.wait().await.map_err(PublisherError::Retryable)?;
        if failed.is_empty() {
            return Ok(sent);
        }
        match sent {
            SentBatch::Ack => Ok(SentBatch::Partial {
                responses: None,
                failed,
            }),
            SentBatch::Partial {
                responses,
                failed: mut inner_failed,
            } => {
                inner_failed.extend(failed);
                Ok(SentBatch::Partial {
                    responses,
                    failed: inner_failed,
                })
            }
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Folds each received batch before the handler sees it. A message that cannot be
/// folded is logged and dropped: acked, or nacked under `on_error: fail`.
pub struct AggregateConsumer {
    inner: Box<dyn MessageConsumer>,
    aggregate: Aggregate,
}

impl AggregateConsumer {
    pub async fn new(
        inner: Box<dyn MessageConsumer>,
        config: &AggregateMiddleware,
        route_name: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            aggregate: Aggregate::connect(config, route_name).await?,
        })
    }
}

#[async_trait]
impl MessageConsumer for AggregateConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }

    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            let ReceivedBatch { messages, commit } = self.inner.receive_batch(max_messages).await?;
            if messages.is_empty() {
                return Ok(ReceivedBatch { messages, commit });
            }
            let len = messages.len();
            let (results, flush) = match self.aggregate.fold_batch(messages).await {
                Ok(results) => results,
                Err(e) => {
                    if let Err(nack) = commit(vec![MessageDisposition::Nack; len]).await {
                        tracing::warn!("aggregate: failed to nack the batch: {nack}");
                    }
                    return Err(ConsumerError::Connection(e));
                }
            };
            let fail = self.aggregate.on_error == AggregateOnError::Fail;
            let mut folded = Vec::with_capacity(len);
            let mut kept = Vec::with_capacity(len);
            for (i, result) in results.into_iter().enumerate() {
                match result {
                    Ok(m) => {
                        folded.push(m);
                        kept.push(i);
                    }
                    Err((m, e)) => tracing::error!(
                        message_id = format_args!("{:032x}", m.message_id),
                        "aggregate: {} input message: {e:#}",
                        if fail { "rejecting" } else { "dropping" }
                    ),
                }
            }
            let failed = move || match fail {
                true => MessageDisposition::Nack,
                false => MessageDisposition::Ack,
            };
            if folded.is_empty() {
                commit(vec![failed(); len])
                    .await
                    .map_err(ConsumerError::Connection)?;
                continue;
            }
            // Failed messages are acked, or nacked under `on_error: fail`; the kept ones
            // take the route's dispositions.
            let commit: BatchCommitFunc = match kept.len() == len {
                true => commit,
                false => Box::new(move |dispositions| {
                    let mut all = vec![failed(); len];
                    for (i, d) in kept.into_iter().zip(dispositions) {
                        all[i] = d;
                    }
                    commit(all)
                }),
            };
            // The ack waits until the states this batch changed are stored.
            let commit: BatchCommitFunc = match flush.0.is_some() {
                false => commit,
                true => Box::new(move |dispositions| {
                    Box::pin(async move {
                        if let Err(e) = flush.wait().await {
                            let nacks = vec![MessageDisposition::Nack; dispositions.len()];
                            if let Err(nack) = commit(nacks).await {
                                tracing::warn!("aggregate: failed to nack the batch: {nack}");
                            }
                            return Err(e);
                        }
                        commit(dispositions).await
                    })
                }),
            };
            return Ok(ReceivedBatch {
                messages: folded,
                commit,
            });
        }
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests;
