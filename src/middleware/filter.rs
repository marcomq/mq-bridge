//! Expression predicates: the `filter` middleware and the `switch` endpoint's
//! `when` cases.
//!
//! An expression reads the payload's top-level JSON fields as bare names
//! (`amount > 100`) and the message metadata under the reserved `meta` prefix
//! (`meta.http_status_code == '200'`). Metadata and text-typed columns arrive as
//! strings, but comparing one against a numeric literal reads it as a number, so
//! `meta.retries > 3` needs no cast. `number()` stays accepted, and is still
//! required where no literal names the intent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use serde_json::{Map, Value};
use zen_expression::compiler::{Compare, FetchFastTarget, Jump, Opcode};
use zen_expression::expression::Standard;
use zen_expression::{compile_expression, Expression, Variable};

use super::deferred_commit::{run_all, DeferredCommits};
use super::raw_json::RawPairs;
use crate::traits::{
    BatchCommitFunc, BoxFuture, CommitFunc, ConsumerError, EndpointStatus, MessageConsumer,
    MessageDisposition, MessagePublisher, PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;

/// The reserved context key under which message metadata is exposed.
///
/// It shadows a payload field of the same name.
const METADATA_PREFIX: &str = "meta";

/// A compiled predicate over a message's payload and metadata.
pub(crate) struct CompiledFilter {
    expression: Expression<Standard>,
    fast_predicate: Option<FastPredicate>,
    /// Whether every term of `fast_predicate` reads a metadata key or a single top-level
    /// payload field, which is what the span route can serve without a document.
    fast_reads_spans: bool,
    /// Whether `fast_predicate` reads the payload at all. A metadata-only predicate must
    /// not require the payload to be JSON.
    fast_reads_payload: bool,
    /// Payload field paths the expression reads, e.g. `["order", "status"]`.
    payload_paths: Vec<Vec<String>>,
    /// Metadata keys the expression reads via the `meta` prefix.
    metadata_keys: Vec<String>,
    /// Dotted paths the expression compares against a numeric literal.
    numeric_paths: Vec<String>,
    uses_all_metadata: bool,
    warned_unusable_field: AtomicBool,
}

/// A tree of single-field comparisons joined by `and`/`or`, decided without building a
/// `Value` for the payload or running the expression VM.
///
/// Every evaluation returns `Option<bool>`: `None` means the slow path has to decide,
/// which happens only where it would raise an error this route cannot phrase.
enum FastPredicate {
    Term(FastTerm),
    And(Box<FastPredicate>, Box<FastPredicate>),
    Or(Box<FastPredicate>, Box<FastPredicate>),
}

/// One field tested against one literal.
struct FastTerm {
    /// Dotted path into the merged document, so metadata reads as `["meta", key]`.
    path: Vec<String>,
    op: FastOp,
    literal: FastLiteral,
}

#[derive(Clone, Copy)]
enum FastOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

enum FastLiteral {
    Null,
    Bool(bool),
    Number(rust_decimal::Decimal),
    String(Arc<str>),
}

/// What a term found where it looked. Metadata is always text, a payload field is
/// whatever JSON held, and either can be missing.
#[derive(Clone, Copy)]
enum Reading<'a> {
    Absent,
    Text(&'a str),
    Json(&'a Value),
}

impl FastOp {
    fn from_compare(compare: Compare) -> Self {
        match compare {
            Compare::More => Self::Gt,
            Compare::MoreOrEqual => Self::Ge,
            Compare::Less => Self::Lt,
            Compare::LessOrEqual => Self::Le,
        }
    }

    /// The same test with its operands swapped, for a literal written on the left.
    fn flipped(self) -> Self {
        match self {
            Self::Lt => Self::Gt,
            Self::Gt => Self::Lt,
            Self::Le => Self::Ge,
            Self::Ge => Self::Le,
            other => other,
        }
    }

    fn is_ordering(self) -> bool {
        !matches!(self, Self::Eq | Self::Ne)
    }

    fn accepts(self, ordering: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::{Equal, Greater, Less};
        match self {
            Self::Eq => ordering == Equal,
            Self::Ne => ordering != Equal,
            Self::Lt => ordering == Less,
            Self::Le => ordering != Greater,
            Self::Gt => ordering == Greater,
            Self::Ge => ordering != Less,
        }
    }
}

impl Reading<'_> {
    /// Absent, null, or a container: what [`resolve`] refuses to hand the expression.
    fn is_unusable(&self) -> bool {
        match self {
            Self::Absent => true,
            Self::Text(_) => false,
            Self::Json(value) => value.is_null() || value.is_array() || value.is_object(),
        }
    }
}

impl FastTerm {
    fn metadata_key(&self) -> Option<&str> {
        (self.path.len() == 2 && self.path[0] == METADATA_PREFIX).then(|| self.path[1].as_str())
    }

    fn payload_key(&self) -> Option<&str> {
        (self.path.len() == 1 && self.path[0] != METADATA_PREFIX).then(|| self.path[0].as_str())
    }

    /// `None` where the engine would raise an error. What an error means depends on the
    /// rest of the expression — it aborts the whole evaluation rather than just this
    /// term, and `CompiledFilter::evaluate` then weighs it against every field the
    /// expression reads — so those cases belong to the slow path alone.
    fn evaluate(&self, reading: Reading<'_>) -> Option<bool> {
        match self.op {
            // Equality never errors: mismatched types simply do not match.
            FastOp::Eq => Some(self.equals(reading)),
            FastOp::Ne => Some(!self.equals(reading)),
            _ => {
                let FastLiteral::Number(expected) = &self.literal else {
                    return None;
                };
                let actual = Self::numeric(reading)?;
                Some(self.op.accepts(actual.cmp(expected)))
            }
        }
    }

    /// Equality as the engine performs it: mismatched types simply do not match, and a
    /// text-typed field compared against a number is read as one.
    fn equals(&self, reading: Reading<'_>) -> bool {
        match (&self.literal, reading) {
            (FastLiteral::Null, Reading::Absent | Reading::Json(Value::Null)) => true,
            (FastLiteral::Bool(expected), Reading::Json(Value::Bool(actual))) => expected == actual,
            (FastLiteral::String(expected), Reading::Json(Value::String(actual))) => {
                expected.as_ref() == actual
            }
            (FastLiteral::String(expected), Reading::Text(actual)) => expected.as_ref() == actual,
            (FastLiteral::Number(expected), Reading::Json(Value::Number(actual))) => {
                Variable::from(&Value::Number(actual.clone()))
                    .as_number()
                    .is_some_and(|actual| actual == *expected)
            }
            (FastLiteral::Number(expected), Reading::Json(Value::String(actual))) => {
                parse_number(actual).is_some_and(|actual| actual == *expected)
            }
            (FastLiteral::Number(expected), Reading::Text(actual)) => {
                parse_number(actual).is_some_and(|actual| actual == *expected)
            }
            _ => false,
        }
    }

    /// The value as a number, reading a text-typed field as one the way the slow path's
    /// `coerce_numeric_fields` does. `None` for anything the engine cannot order.
    fn numeric(reading: Reading<'_>) -> Option<rust_decimal::Decimal> {
        let text = match reading {
            Reading::Text(text) => text,
            Reading::Json(Value::String(text)) => text,
            Reading::Json(Value::Number(number)) => {
                return Variable::from(&Value::Number(number.clone())).as_number()
            }
            Reading::Absent | Reading::Json(_) => return None,
        };
        parse_number(text)
    }
}

impl FastPredicate {
    /// Evaluates against the merged document the slow path builds, which already carries
    /// metadata under `meta` and a `null` for every absent path.
    fn evaluate(&self, document: &Value) -> Option<bool> {
        match self {
            Self::Term(term) => term
                .evaluate(resolve_any(document, &term.path).map_or(Reading::Absent, Reading::Json)),
            // Short-circuiting mirrors the engine's own `and`/`or`, so a term the fast
            // route would defer on is skipped here exactly as it is there.
            Self::And(left, right) => match left.evaluate(document)? {
                false => Some(false),
                true => right.evaluate(document),
            },
            Self::Or(left, right) => match left.evaluate(document)? {
                true => Some(true),
                false => right.evaluate(document),
            },
        }
    }

    /// Evaluates from the record's top-level spans, so only the fields the predicate
    /// actually names are ever parsed.
    fn evaluate_spans(
        &self,
        message: &CanonicalMessage,
        pairs: &[(&str, &serde_json::value::RawValue)],
        warn: &mut dyn FnMut(&str),
    ) -> Option<bool> {
        match self {
            Self::Term(term) => Self::evaluate_term_from_spans(term, message, pairs, warn),
            Self::And(left, right) => match left.evaluate_spans(message, pairs, warn)? {
                false => Some(false),
                true => right.evaluate_spans(message, pairs, warn),
            },
            Self::Or(left, right) => match left.evaluate_spans(message, pairs, warn)? {
                true => Some(true),
                false => right.evaluate_spans(message, pairs, warn),
            },
        }
    }

    fn evaluate_term_from_spans(
        term: &FastTerm,
        message: &CanonicalMessage,
        pairs: &[(&str, &serde_json::value::RawValue)],
        warn: &mut dyn FnMut(&str),
    ) -> Option<bool> {
        if let Some(key) = term.metadata_key() {
            let reading = message
                .metadata
                .get(key)
                .map_or(Reading::Absent, |value| Reading::Text(value));
            if reading.is_unusable() {
                warn(&format!("{METADATA_PREFIX}.{key}"));
            }
            return term.evaluate(reading);
        }

        let key = term.payload_key()?;
        // Searching from the back resolves a duplicated key to its last value, the way
        // collapsing the payload into a `Value` would.
        let raw = pairs
            .iter()
            .rev()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, raw)| *raw);
        let value: Option<Value> = match raw {
            // A field that will not parse is left for the slow path to report.
            Some(raw) => Some(serde_json::from_str(raw.get()).ok()?),
            None => None,
        };
        let reading = value.as_ref().map_or(Reading::Absent, Reading::Json);
        if reading.is_unusable() {
            warn(key);
        }
        term.evaluate(reading)
    }

    /// Whether every term reads a metadata key or a single top-level payload field.
    fn reads_spans(&self) -> bool {
        match self {
            Self::Term(term) => term.metadata_key().is_some() || term.payload_key().is_some(),
            Self::And(left, right) | Self::Or(left, right) => {
                left.reads_spans() && right.reads_spans()
            }
        }
    }

    fn reads_payload(&self) -> bool {
        match self {
            Self::Term(term) => term.metadata_key().is_none(),
            Self::And(left, right) | Self::Or(left, right) => {
                left.reads_payload() || right.reads_payload()
            }
        }
    }
}

/// Lazily prepared input shared by predicate cases.
pub(crate) struct FilterContext {
    document: Value,
    payload_loaded: bool,
}

impl FilterContext {
    pub(crate) fn new() -> Self {
        Self {
            document: Value::Object(Map::new()),
            payload_loaded: false,
        }
    }

    fn load_payload(&mut self, message: &CanonicalMessage) -> anyhow::Result<()> {
        if self.payload_loaded {
            return Ok(());
        }
        let mut document: Value = serde_json::from_slice(message.payload.as_ref())
            .context("filter requires a structured JSON object payload")?;
        let object = document
            .as_object_mut()
            .context("filter requires a structured JSON object payload")?;
        if let Some(meta) = self
            .document
            .as_object_mut()
            .and_then(|current| current.remove(METADATA_PREFIX))
        {
            object.insert(METADATA_PREFIX.to_string(), meta);
        }
        self.document = document;
        self.payload_loaded = true;
        Ok(())
    }

    fn add_metadata(&mut self, message: &CanonicalMessage, keys: &[String], all: bool) {
        if keys.is_empty() && !all {
            return;
        }
        let object = self
            .document
            .as_object_mut()
            .expect("filter context is an object");
        let meta = object
            .entry(METADATA_PREFIX)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("filter metadata context is an object");
        // Overwrites: `meta` shadows a payload field of the same name, and re-inserting
        // the same message's metadata on a reused context is idempotent.
        if all {
            for (key, value) in &message.metadata {
                meta.insert(key.clone(), Value::String(value.clone()));
            }
        } else {
            for key in keys {
                let value = message
                    .metadata
                    .get(key)
                    .map_or(Value::Null, |value| Value::String(value.clone()));
                meta.insert(key.clone(), value);
            }
        }
    }
}

impl CompiledFilter {
    pub(crate) fn new(expression: &str) -> anyhow::Result<Self> {
        let normalized = normalize_expression(expression);
        let expression =
            compile_expression(&normalized).map_err(|error| anyhow!(error.to_string()))?;
        let fast_predicate = compile_fast_predicate(&expression);
        let (payload_paths, metadata_keys, uses_all_metadata, has_unsupported_path) =
            referenced_paths(&expression);
        let numeric_paths = numerically_compared_paths(&expression);
        // An indexed path never resolves, so tolerating it per message would drop every
        // message while the route reported itself healthy. Refuse to start instead.
        if has_unsupported_path {
            bail!(
                "filter expression uses an indexed path, which is unsupported; \
                 index into the array before the filter, or compare a scalar field"
            );
        }
        let fast_reads_spans = fast_predicate
            .as_ref()
            .is_some_and(FastPredicate::reads_spans);
        let fast_reads_payload = fast_predicate
            .as_ref()
            .is_some_and(FastPredicate::reads_payload);
        Ok(Self {
            expression,
            fast_predicate,
            fast_reads_spans,
            fast_reads_payload,
            payload_paths,
            metadata_keys,
            numeric_paths,
            uses_all_metadata,
            warned_unusable_field: AtomicBool::new(false),
        })
    }

    /// Whether this message satisfies the expression.
    ///
    /// A field that is absent, null, or not a scalar means "does not match", the
    /// way a SQL `WHERE` treats NULL: one heterogeneous document should not end a
    /// route that is otherwise running fine. A payload that is not a JSON object
    /// is still an error, because that means the expression was pointed at data
    /// it cannot read at all, and silently dropping everything would be worse.
    pub(crate) fn matches(&self, message: &CanonicalMessage) -> anyhow::Result<bool> {
        self.matches_with_context(message, &mut FilterContext::new())
    }

    pub(crate) fn matches_with_context(
        &self,
        message: &CanonicalMessage,
        context: &mut FilterContext,
    ) -> anyhow::Result<bool> {
        if let Some(predicate) = &self.fast_predicate {
            if self.fast_reads_spans {
                let mut warn = |field: &str| self.warn_unusable_field(field);
                // A metadata-only predicate must not require the payload to be JSON.
                let decided = if self.fast_reads_payload {
                    match serde_json::from_slice(&message.payload) {
                        Ok(RawPairs(pairs)) => predicate.evaluate_spans(message, &pairs, &mut warn),
                        Err(_) => None,
                    }
                } else {
                    predicate.evaluate_spans(message, &[], &mut warn)
                };
                if let Some(result) = decided {
                    return Ok(result);
                }
            }
        }

        if !self.payload_paths.is_empty() {
            context.load_payload(message)?;
        }

        let mut has_unusable_field = false;
        let mut synthesized = Vec::new();
        for path in &self.payload_paths {
            if resolve(&context.document, path).is_none() {
                has_unusable_field = true;
                self.warn_unusable_field(&path.join("."));
                if let Some(created) = insert_null_if_absent(&mut context.document, path) {
                    synthesized.push(created);
                }
            }
        }

        for key in &self.metadata_keys {
            if !message.metadata.contains_key(key) {
                has_unusable_field = true;
                self.warn_unusable_field(&format!("{METADATA_PREFIX}.{key}"));
            }
        }

        context.add_metadata(message, &self.metadata_keys, self.uses_all_metadata);

        let result = self.evaluate(&context.document, has_unusable_field);
        // A context is shared across a `switch`'s predicates, so the nulls this one
        // synthesized must not make the next one see an object where a field is absent.
        for path in synthesized {
            remove_path(&mut context.document, &path);
        }
        result
    }

    fn evaluate(&self, document: &Value, has_unusable_field: bool) -> anyhow::Result<bool> {
        // Unusable fields were already reported by the caller, which also filled every
        // absent path with a `null`, so this sees the same document the engine would.
        if let Some(result) = self
            .fast_predicate
            .as_ref()
            .and_then(|p| p.evaluate(document))
        {
            return Ok(result);
        }

        let variable = Variable::from(document);
        self.coerce_numeric_fields(&variable);

        let evaluated = match self.expression.evaluate(variable) {
            Ok(evaluated) => evaluated,
            Err(_) if has_unusable_field => return Ok(false),
            Err(error) => {
                let mut text_fields = self
                    .payload_paths
                    .iter()
                    .filter(|path| resolve(document, path).is_some_and(Value::is_string))
                    .map(|path| path.join("."))
                    .collect::<Vec<_>>();
                text_fields.extend(
                    self.metadata_keys
                        .iter()
                        .map(|key| format!("{METADATA_PREFIX}.{key}")),
                );
                text_fields.retain(|field| !self.reads_as_number(document, field));
                return Err(text_typed_field_error(&error.to_string(), &text_fields));
            }
        };
        match evaluated {
            Variable::Bool(value) => Ok(value),
            _ => bail!("filter expression did not evaluate to a boolean"),
        }
    }

    /// Whether [`Self::coerce_numeric_fields`] already made this field a number,
    /// which means it is not what the failed evaluation tripped over.
    fn reads_as_number(&self, document: &Value, field: &str) -> bool {
        self.numeric_paths.iter().any(|path| path == field)
            && resolve(
                document,
                &field.split('.').map(String::from).collect::<Vec<_>>(),
            )
            .and_then(Value::as_str)
            .is_some_and(|text| parse_number(text).is_some())
    }

    /// Reads a text field the expression compares against a number as a number.
    ///
    /// Sources that type everything as text (CSV, metadata, SQL `numeric`) would
    /// otherwise need `number()` on every field. Text that is not a number is left
    /// alone, so it still reaches [`text_typed_field_error`].
    ///
    /// This rewrites the per-evaluation [`Variable`], never the document the
    /// caller's [`FilterContext`] shares between a `switch`'s cases.
    fn coerce_numeric_fields(&self, root: &Variable) {
        for path in &self.numeric_paths {
            let Some(value) = root.dot(path) else {
                continue;
            };
            let Some(text) = value.as_str() else { continue };
            if let Some(number) = parse_number(text) {
                root.dot_insert(path, Variable::Number(number));
            }
        }
    }

    /// Warns once per route: a field that is never usable drops every message,
    /// and a typo in the expression should not just look like an empty source.
    fn warn_unusable_field(&self, field: &str) {
        if !self.warned_unusable_field.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                field,
                "filter field is absent, null, or not a scalar; those messages do not match"
            );
        }
    }
}

/// Recognises the shapes the fast route can serve: one field against one literal, joined
/// by `and`/`or`.
///
/// `and` and `or` compile to `[<left>, Jump(IfFalse|IfTrue, j), Pop, <right>]`, where the
/// jump always lands on the last opcode of the subexpression it belongs to. The first
/// such jump is therefore the outermost operator, which is what makes the grouping
/// recoverable — and why `a or b and c` cannot be mistaken for `(a or b) and c`.
fn compile_fast_predicate(expression: &Expression<Standard>) -> Option<FastPredicate> {
    parse_fast_node(expression.bytecode().as_ref())
}

fn parse_fast_node(opcodes: &[Opcode]) -> Option<FastPredicate> {
    let last = opcodes.len().checked_sub(1)?;
    for (index, opcode) in opcodes.iter().enumerate() {
        let Opcode::Jump(kind @ (Jump::IfFalse | Jump::IfTrue), offset) = opcode else {
            continue;
        };
        if index + *offset as usize != last || !matches!(opcodes.get(index + 1), Some(Opcode::Pop))
        {
            continue;
        }
        let left = Box::new(parse_fast_node(&opcodes[..index])?);
        let right = Box::new(parse_fast_node(&opcodes[index + 2..])?);
        return Some(match kind {
            Jump::IfFalse => FastPredicate::And(left, right),
            _ => FastPredicate::Or(left, right),
        });
    }
    parse_fast_term(opcodes).map(FastPredicate::Term)
}

fn parse_fast_term(opcodes: &[Opcode]) -> Option<FastTerm> {
    let (left, right, op) = match opcodes {
        [left, right, Opcode::Equal] => (left, right, FastOp::Eq),
        [left, right, Opcode::Equal, Opcode::Not] => (left, right, FastOp::Ne),
        [left, right, Opcode::Compare(compare)] => (left, right, FastOp::from_compare(*compare)),
        _ => return None,
    };

    let (path, literal, op) = match parse_fast_fetch(left).zip(parse_fast_literal(right)) {
        Some((path, literal)) => (path, literal, op),
        // A literal written on the left reverses the test.
        None => {
            let (literal, path) = parse_fast_literal(left).zip(parse_fast_fetch(right))?;
            (path, literal, op.flipped())
        }
    };
    if path.is_empty() {
        return None;
    }
    // The engine compares only numbers, and reports anything else as a typed-field
    // problem the slow path phrases. Ordering against another kind of literal is
    // therefore left to it entirely.
    if op.is_ordering() && !matches!(literal, FastLiteral::Number(_)) {
        return None;
    }
    Some(FastTerm { path, op, literal })
}

fn parse_fast_fetch(opcode: &Opcode) -> Option<Vec<String>> {
    match opcode {
        Opcode::FetchEnv(name) => Some(vec![name.to_string()]),
        Opcode::FetchFast(targets) => targets
            .iter()
            .map(|target| match target {
                FetchFastTarget::Root | FetchFastTarget::Begin => Some(None),
                FetchFastTarget::String(name) => Some(Some(name.to_string())),
                FetchFastTarget::Number(_) => None,
            })
            .collect::<Option<Vec<_>>>()
            .map(|segments| segments.into_iter().flatten().collect()),
        _ => None,
    }
}

fn parse_fast_literal(opcode: &Opcode) -> Option<FastLiteral> {
    match opcode {
        Opcode::PushNull => Some(FastLiteral::Null),
        Opcode::PushBool(value) => Some(FastLiteral::Bool(*value)),
        Opcode::PushNumber(value) => Some(FastLiteral::Number(*value)),
        Opcode::PushString(value) => Some(FastLiteral::String(value.clone())),
        _ => None,
    }
}

/// Dotted paths the expression compares against a numeric literal.
///
/// The literal is what proves the intent: `amount > 100` means the field is a
/// number even where the source delivered it as text, while `amount == '100'`
/// asks for a string and `meta.a > meta.b` may well be comparing dates. Anything
/// but a bare field against a bare number is left for `number()`.
fn numerically_compared_paths(expression: &Expression<Standard>) -> Vec<String> {
    let opcodes = expression.bytecode();
    let mut paths: Vec<String> = Vec::new();

    for window in opcodes.as_ref().windows(3) {
        if !matches!(window[2], Opcode::Compare(_) | Opcode::Equal) {
            continue;
        }
        let fetch = match (&window[0], &window[1]) {
            (Opcode::PushNumber(_), operand) | (operand, Opcode::PushNumber(_)) => operand,
            _ => continue,
        };
        let Some(path) = parse_fast_fetch(fetch).filter(|path| !path.is_empty()) else {
            continue;
        };
        let dotted = path.join(".");
        if !paths.contains(&dotted) {
            paths.push(dotted);
        }
    }

    paths
}

/// Parses exactly what the engine's own `number()` accepts, so the implicit and
/// the explicit cast cannot disagree.
fn parse_number(text: &str) -> Option<rust_decimal::Decimal> {
    let text = text.trim();
    rust_decimal::Decimal::from_str_exact(text)
        .or_else(|_| rust_decimal::Decimal::from_scientific(text))
        .ok()
}

/// Walks a dotted path, yielding the value only if it is a usable scalar.
fn resolve<'a>(document: &'a Value, path: &[String]) -> Option<&'a Value> {
    let current = resolve_any(document, path)?;
    let usable = !current.is_null() && !current.is_array() && !current.is_object();
    usable.then_some(current)
}

fn resolve_any<'a>(document: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut current = document;
    for segment in path {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

/// Makes a genuinely absent path visible to the expression VM as `null`.
///
/// Returns the shallowest prefix it had to create, so the caller can undo the whole
/// insertion with [`remove_path`].
fn insert_null_if_absent(document: &mut Value, path: &[String]) -> Option<Vec<String>> {
    let mut created = None;
    let mut current = document;
    for (index, segment) in path.iter().enumerate() {
        let Some(object) = current.as_object_mut() else {
            return created;
        };
        if created.is_none() && !object.contains_key(segment.as_str()) {
            created = Some(path[..=index].to_vec());
        }
        if index + 1 == path.len() {
            object.entry(segment.clone()).or_insert(Value::Null);
            return created;
        }
        current = object
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    created
}

/// Removes what [`insert_null_if_absent`] added.
fn remove_path(document: &mut Value, path: &[String]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = document;
    for segment in parents {
        let Some(next) = current.as_object_mut().and_then(|o| o.get_mut(segment)) else {
            return;
        };
        current = next;
    }
    if let Some(object) = current.as_object_mut() {
        object.remove(last.as_str());
    }
}

/// Splits the paths the compiled expression reads into payload and metadata.
fn referenced_paths(
    expression: &Expression<Standard>,
) -> (Vec<Vec<String>>, Vec<String>, bool, bool) {
    let mut payload = Vec::new();
    let mut metadata = Vec::new();
    let mut uses_all_metadata = false;
    let mut has_unsupported_path = false;

    for opcode in expression.bytecode().iter() {
        let path: Vec<String> = match opcode {
            Opcode::FetchEnv(name) => vec![name.to_string()],
            Opcode::FetchFast(targets) => {
                if targets
                    .iter()
                    .any(|target| matches!(target, FetchFastTarget::Number(_)))
                {
                    has_unsupported_path = true;
                    continue;
                }
                targets
                    .iter()
                    .filter_map(|target| match target {
                        FetchFastTarget::String(name) => Some(name.to_string()),
                        FetchFastTarget::Root | FetchFastTarget::Begin => None,
                        FetchFastTarget::Number(_) => {
                            unreachable!("numeric targets were rejected above")
                        }
                    })
                    .collect()
            }
            _ => continue,
        };
        if path.is_empty() {
            continue;
        }

        if path[0] == METADATA_PREFIX {
            // A bare `meta` with no key reads the whole map; nothing to check.
            if let Some(key) = path.get(1) {
                if !metadata.contains(key) {
                    metadata.push(key.clone());
                }
            } else {
                uses_all_metadata = true;
            }
        } else if !payload.contains(&path) {
            payload.push(path);
        }
    }

    (payload, metadata, uses_all_metadata, has_unsupported_path)
}

/// Turns the engine's opcode-level type error into one naming the field and the fix.
///
/// A text-typed column compares a string against a number, which the expression VM
/// reports only as `Opcode Compare: Unsupported type`. That names neither the column
/// nor `number()`, so the route looks broken rather than under-specified.
///
/// A bare numeric literal is read as an implicit cast, so what reaches here is text
/// no cast can rescue (`"n/a" > 100`) or a comparison with no literal to take the
/// intent from (`meta.a > meta.b`, `amount > 100 * 2`).
///
/// Which fields arrive as text depends on the source, so the hint names both
/// shapes rather than asserting one: CSV and most key-value stores type
/// everything as text, while a SQL source types most columns natively and
/// delivers only `numeric`/`timestamptz` as strings. Metadata is always text.
fn text_typed_field_error(error: &str, text_fields: &[String]) -> anyhow::Error {
    let Some(first) = text_fields.first() else {
        return anyhow!(error.to_string());
    };
    let fields = text_fields.join("`, `");
    anyhow!(
        "{error}; filter field `{fields}` holds text, not a number — compare it as \
         `number({first})` (metadata is always text, as are all CSV fields; SQL sources \
         deliver numeric and timestamp columns as strings)"
    )
}

/// Rewrites `&&` and `||` to the `and`/`or` the expression engine accepts.
///
/// The engine's lexer rejects the C-style spellings outright, and reaching for
/// them is the first thing anyone does.
fn normalize_expression(expression: &str) -> String {
    let mut normalized = String::with_capacity(expression.len());
    let mut chars = expression.chars().peekable();
    let mut quote = None;
    let mut escaped = false;

    while let Some(character) = chars.next() {
        if let Some(delimiter) = quote {
            normalized.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' => {
                quote = Some(character);
                normalized.push(character);
            }
            '&' if chars.next_if_eq(&'&').is_some() => normalized.push_str(" and "),
            '|' if chars.next_if_eq(&'|').is_some() => normalized.push_str(" or "),
            _ => normalized.push(character),
        }
    }

    normalized
}

/// Drops messages that do not match, before anything downstream sees them.
pub struct FilterConsumer {
    inner: Box<dyn MessageConsumer>,
    filter: Arc<CompiledFilter>,
    deferred: DeferredCommits,
}

impl FilterConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, expression: &str) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            filter: Arc::new(CompiledFilter::new(expression).context("invalid filter expression")?),
            deferred: DeferredCommits::new(),
        })
    }
}

#[async_trait]
impl MessageConsumer for FilterConsumer {
    /// Reads until a message is kept, holding the acks for what it dropped.
    ///
    /// The caller may ask for the next message before committing this one, so on a
    /// source with cumulative acks an inline drop ack would jump ahead of a retained
    /// message the caller still holds. Those acks run from inside its commit instead,
    /// exactly as [`Self::receive_batch`] does.
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        loop {
            let received = self.inner.receive().await?;
            if self
                .filter
                .matches(&received.message)
                .map_err(ConsumerError::Permanent)?
            {
                let held = self.deferred.take();
                if held.is_empty() {
                    return Ok(received);
                }
                let inner_commit = received.commit;
                let commit: CommitFunc = Box::new(move |disposition| {
                    Box::pin(async move {
                        run_all(held).await?;
                        inner_commit(disposition).await
                    })
                });
                return Ok(Received {
                    message: received.message,
                    commit,
                });
            }

            let ordered = self.inner.commit_requires_order();
            let dropped_commit = received.commit;
            let commit: BatchCommitFunc = Box::new(move |dispositions| {
                dropped_commit(
                    dispositions
                        .into_iter()
                        .next()
                        .unwrap_or(MessageDisposition::Ack),
                )
            });
            self.deferred
                .ack_emptied(ordered, commit, 1)
                .await
                .map_err(ConsumerError::Connection)?;
        }
    }

    /// Reads until a batch has something to keep, acknowledging what it drops.
    ///
    /// An empty batch is the drain signal, and nothing follows it to carry a held
    /// commit — so they are flushed there instead. A drain that fails to flush them
    /// simply re-reads and re-drops those messages next run; nothing reaches the
    /// destination twice.
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let target = max_messages.max(1);
        let mut messages = Vec::with_capacity(target);
        let mut commits: Vec<(usize, BatchCommitFunc)> = Vec::new();

        loop {
            let requested = target - messages.len();
            let batch = self.inner.receive_batch(requested).await?;
            if batch.messages.is_empty() {
                let held = self.deferred.take();
                if messages.is_empty() {
                    run_all(held).await.map_err(|error| {
                        ConsumerError::Connection(
                            error.context(
                                "failed to flush deferred filter acknowledgements on drain",
                            ),
                        )
                    })?;
                    return Ok(batch);
                }

                // Trailing filtered-out source batches must commit after the retained
                // batches already collected in this call, never ahead of them.
                let drain_commit = batch.commit;
                commits.push((
                    0,
                    Box::new(move |_| {
                        Box::pin(async move {
                            run_all(held).await?;
                            drain_commit(Vec::new()).await
                        })
                    }),
                ));
                break;
            }

            let source_count = batch.messages.len();
            let mut kept = Vec::with_capacity(source_count);
            let mut keep_flags = Vec::with_capacity(batch.messages.len());
            for message in batch.messages {
                let keep = self
                    .filter
                    .matches(&message)
                    .map_err(ConsumerError::Permanent)?;
                keep_flags.push(keep);
                if keep {
                    kept.push(message);
                }
            }

            if kept.is_empty() {
                let ordered = self.inner.commit_requires_order();
                self.deferred
                    .ack_emptied(ordered, batch.commit, keep_flags.len())
                    .await
                    .map_err(ConsumerError::Connection)?;
                continue;
            }

            let held = self.deferred.take();
            let expected = kept.len();
            let commit: BatchCommitFunc = Box::new(move |dispositions| {
                Box::pin(async move {
                    if dispositions.len() != expected {
                        bail!(
                            "filter commit received {} dispositions for {expected} retained messages",
                            dispositions.len()
                        );
                    }
                    run_all(held).await?;
                    let mut retained = dispositions.into_iter();
                    let expanded = keep_flags
                        .into_iter()
                        .map(|keep| {
                            if keep {
                                retained.next().unwrap_or(MessageDisposition::Nack)
                            } else {
                                MessageDisposition::Ack
                            }
                        })
                        .collect();
                    (batch.commit)(expanded).await
                })
            });
            messages.extend(kept);
            commits.push((expected, commit));

            // A short source batch is the transport's natural flush boundary. Do
            // not turn filtering into an unbounded wait on a live source.
            if messages.len() >= target || source_count < requested {
                break;
            }
        }

        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            Box::pin(async move {
                let expected: usize = commits.iter().map(|(count, _)| count).sum();
                if dispositions.len() != expected {
                    bail!(
                        "filter commit received {} dispositions for {expected} retained messages",
                        dispositions.len()
                    );
                }

                let mut offset = 0;
                for (count, commit) in commits {
                    let end = offset + count;
                    commit(dispositions[offset..end].to_vec()).await?;
                    offset = end;
                }
                Ok(())
            })
        });
        Ok(ReceivedBatch { messages, commit })
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }

    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    /// Releases any commit still held for an emptied batch before the source goes away.
    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        let inner_hook = self.inner.on_disconnect_hook();
        let held = self.deferred.take_shared();
        if held.is_empty() {
            return inner_hook;
        }
        Some(Box::pin(async move {
            let mut first_error = run_all(held).await.err();
            if let Some(hook) = inner_hook {
                if let Err(error) = hook.await {
                    first_error.get_or_insert(error);
                }
            }
            first_error.map_or(Ok(()), Err)
        }))
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        self.inner.close().await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Drops messages that do not match instead of publishing them.
///
/// Dropped messages count as sent: the route did what the configuration asked.
pub struct FilterPublisher {
    inner: Box<dyn MessagePublisher>,
    filter: Arc<CompiledFilter>,
}

impl FilterPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, expression: &str) -> anyhow::Result<Self> {
        Ok(Self {
            inner,
            filter: Arc::new(CompiledFilter::new(expression).context("invalid filter expression")?),
        })
    }
}

#[async_trait]
impl MessagePublisher for FilterPublisher {
    /// Delegated, so wrapping a sink in a filter does not swallow its lifecycle. The
    /// route only ever runs the hooks of the *outermost* publisher, and the structural
    /// endpoints rely on theirs to reach their nested destinations.
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.inner.flush().await
    }

    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if self
            .filter
            .matches(&message)
            .map_err(PublisherError::NonRetryable)?
        {
            return self.inner.send(message).await;
        }
        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // Split across cores: an ordered sink serializes this whole call, so route
        // concurrency cannot overlap it and the batch itself is what has to parallelise.
        let filter = Arc::clone(&self.filter);
        let outcomes = crate::support::parallel::map_messages(messages, move |message| {
            filter.matches(&message).map(|kept| kept.then_some(message))
        })
        .await;

        let mut kept = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            match outcome {
                Ok(Some(message)) => kept.push(message),
                Ok(None) => {}
                Err(error) => return Err(PublisherError::NonRetryable(error)),
            }
        }
        if kept.is_empty() {
            return Ok(SentBatch::Ack);
        }
        // `SentBatch::Partial.failed` carries the messages themselves, not
        // indices into the batch, so the dropped ones need no remapping.
        self.inner.send_batch(kept).await
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    async fn status(&self) -> EndpointStatus {
        self.inner.status().await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::collections::HashMap;

    fn message(payload: &str, metadata: &[(&str, &str)]) -> CanonicalMessage {
        CanonicalMessage {
            message_id: 1,
            payload: Bytes::from(payload.to_string()),
            metadata: metadata
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn a_payload_field_is_compared_by_value() {
        let filter = CompiledFilter::new("x > 100").unwrap();
        assert!(filter.matches(&message(r#"{"x": 150}"#, &[])).unwrap());
        assert!(!filter.matches(&message(r#"{"x": 50}"#, &[])).unwrap());
    }

    #[test]
    fn metadata_reads_through_the_meta_prefix() {
        let filter = CompiledFilter::new("meta.http_status_code == '200'").unwrap();
        assert!(filter
            .matches(&message("{}", &[("http_status_code", "200")]))
            .unwrap());
        assert!(!filter
            .matches(&message("{}", &[("http_status_code", "404")]))
            .unwrap());
    }

    /// The whole point of splitting the paths: a metadata-only predicate is the
    /// hot path for `switch`, and must not pay for a JSON parse.
    #[test]
    fn a_metadata_only_predicate_never_parses_the_payload() {
        let filter = CompiledFilter::new("meta.kind == 'order'").unwrap();
        assert!(filter.payload_paths.is_empty());
        let not_json = message("this is not JSON at all", &[("kind", "order")]);
        assert!(filter.matches(&not_json).unwrap());
    }

    #[test]
    fn payload_and_metadata_combine_in_one_expression() {
        let filter = CompiledFilter::new("x > 100 and meta.kind == 'order'").unwrap();
        assert!(filter
            .matches(&message(r#"{"x": 150}"#, &[("kind", "order")]))
            .unwrap());
        assert!(!filter
            .matches(&message(r#"{"x": 150}"#, &[("kind", "refund")]))
            .unwrap());
    }

    /// Metadata is always `String`, but the literal in `> 3` says the comparison
    /// is numeric, so the cast is inferred rather than demanded.
    #[test]
    fn a_numeric_metadata_comparison_needs_no_explicit_cast() {
        let bare = CompiledFilter::new("meta.retries > 3").unwrap();
        assert!(bare.matches(&message("{}", &[("retries", "5")])).unwrap());
        assert!(!bare.matches(&message("{}", &[("retries", "2")])).unwrap());

        let cast = CompiledFilter::new("number(meta.retries) > 3").unwrap();
        assert!(cast.matches(&message("{}", &[("retries", "5")])).unwrap());
    }

    /// CSV and SQL `numeric` columns arrive as text the same way metadata does.
    #[test]
    fn a_text_typed_payload_field_compares_numerically() {
        let filter = CompiledFilter::new("amount > 100").unwrap();
        assert!(filter
            .matches(&message(r#"{"amount": "125"}"#, &[]))
            .unwrap());
        assert!(!filter
            .matches(&message(r#"{"amount": "50"}"#, &[]))
            .unwrap());
        assert!(filter
            .matches(&message(r#"{"amount": " 125.5 "}"#, &[]))
            .unwrap());
    }

    /// The implicit cast must be exact where `number()` is, not a float round-trip.
    #[test]
    fn the_implicit_cast_keeps_decimal_precision() {
        let filter = CompiledFilter::new("amount > 1.5").unwrap();
        assert!(!filter
            .matches(&message(r#"{"amount": "1.50"}"#, &[]))
            .unwrap());
        assert!(filter
            .matches(&message(r#"{"amount": "1.51"}"#, &[]))
            .unwrap());
    }

    /// A string literal asks for a string. Coercing here would break leading zeros.
    #[test]
    fn a_string_comparison_is_never_coerced() {
        let filter = CompiledFilter::new("zip == '01234'").unwrap();
        assert!(filter
            .matches(&message(r#"{"zip": "01234"}"#, &[]))
            .unwrap());
        assert!(!filter.matches(&message(r#"{"zip": "1234"}"#, &[])).unwrap());
    }

    /// The fast path skips the VM entirely, so it needs the same rule; before this
    /// it could not match a text field against a number at all.
    #[test]
    fn fast_equality_compares_text_against_a_numeric_literal() {
        let meta = CompiledFilter::new("meta.retries == 3").unwrap();
        assert!(meta.fast_predicate.is_some());
        assert!(meta.matches(&message("{}", &[("retries", "3")])).unwrap());
        assert!(!meta.matches(&message("{}", &[("retries", "4")])).unwrap());

        let payload = CompiledFilter::new("amount == 125").unwrap();
        assert!(payload.fast_predicate.is_some());
        assert!(payload
            .matches(&message(r#"{"amount": "125"}"#, &[]))
            .unwrap());
        assert!(!payload
            .matches(&message(r#"{"amount": "126"}"#, &[]))
            .unwrap());
    }

    /// Text no cast can read still names the field and the cast, and a comparison
    /// with no literal to take its intent from still needs `number()`.
    #[test]
    fn unreadable_text_still_names_the_numeric_cast() {
        let filter = CompiledFilter::new("amount > 100").unwrap();
        let error = filter
            .matches(&message(r#"{"amount": "n/a"}"#, &[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("number(amount)"), "got: {error}");

        let pair = CompiledFilter::new("meta.a > meta.b").unwrap();
        let error = pair
            .matches(&message("{}", &[("a", "5"), ("b", "3")]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("number(meta.a)"), "got: {error}");
    }

    /// A field the implicit cast read fine is not what the evaluation tripped over,
    /// so the diagnostic must not name it alongside the one that failed.
    #[test]
    fn the_diagnostic_names_only_the_field_that_failed() {
        let filter = CompiledFilter::new("amount > 100 and other > 1").unwrap();
        let error = filter
            .matches(&message(r#"{"amount": "125", "other": "n/a"}"#, &[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("`other`"), "got: {error}");
        assert!(!error.contains("`amount`"), "got: {error}");
    }

    /// A `switch` runs every `when` case against one context, so a numeric case must
    /// not leave the field a number for a later case that compares it as text.
    #[test]
    fn coercion_does_not_leak_into_the_next_predicate() {
        let numeric = CompiledFilter::new("amount > 100").unwrap();
        let mut textual = CompiledFilter::new("amount == '125'").unwrap();
        textual.fast_predicate = None;
        let message = message(r#"{"amount": "125"}"#, &[]);

        let mut shared = FilterContext::new();
        assert!(numeric.matches_with_context(&message, &mut shared).unwrap());
        assert!(
            textual.matches_with_context(&message, &mut shared).unwrap(),
            "predicate order changed the answer"
        );
    }

    /// The app's original implementation rejected these outright, because it
    /// checked the *root* of the path and found an object.
    #[test]
    fn a_nested_payload_path_resolves_instead_of_dropping_everything() {
        let filter = CompiledFilter::new("order.status == 'open'").unwrap();
        assert!(filter
            .matches(&message(r#"{"order": {"status": "open"}}"#, &[]))
            .unwrap());
        assert!(!filter
            .matches(&message(r#"{"order": {"status": "shipped"}}"#, &[]))
            .unwrap());
    }

    #[test]
    fn an_absent_or_non_scalar_field_does_not_match() {
        let filter = CompiledFilter::new("x > 100").unwrap();
        assert!(!filter.matches(&message(r#"{"y": 1}"#, &[])).unwrap());
        assert!(!filter.matches(&message(r#"{"x": null}"#, &[])).unwrap());
        assert!(!filter.matches(&message(r#"{"x": [1, 2]}"#, &[])).unwrap());
        assert!(!filter.matches(&message("{}", &[])).unwrap());
    }

    #[test]
    fn an_absent_metadata_key_does_not_match() {
        let filter = CompiledFilter::new("meta.kind == 'order'").unwrap();
        assert!(!filter.matches(&message("{}", &[])).unwrap());
    }

    /// Pointing a payload predicate at data it cannot read at all is an error,
    /// not a silent drop of everything.
    #[test]
    fn an_unstructured_payload_is_an_error() {
        let filter = CompiledFilter::new("x > 100").unwrap();
        assert!(filter.matches(&message("not json", &[])).is_err());
        assert!(filter.matches(&message("[1, 2, 3]", &[])).is_err());
    }

    #[test]
    fn c_style_boolean_operators_are_accepted() {
        assert_eq!(normalize_expression("a > 1 && b < 2"), "a > 1  and  b < 2");
        assert_eq!(normalize_expression("a || b"), "a  or  b");
        let filter = CompiledFilter::new("x > 100 && meta.kind == 'order'").unwrap();
        assert!(filter
            .matches(&message(r#"{"x": 150}"#, &[("kind", "order")]))
            .unwrap());
    }

    /// A `&&` inside a string literal is data, not an operator.
    #[test]
    fn boolean_normalization_leaves_string_literals_alone() {
        assert_eq!(normalize_expression("a == 'x && y'"), "a == 'x && y'");
        let filter = CompiledFilter::new("name == 'Ben && Jerry'").unwrap();
        assert!(filter
            .matches(&message(r#"{"name": "Ben && Jerry"}"#, &[]))
            .unwrap());
    }

    #[test]
    fn a_non_boolean_expression_is_rejected_at_evaluation() {
        let filter = CompiledFilter::new("x + 1").unwrap();
        let error = filter
            .matches(&message(r#"{"x": 1}"#, &[]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("did not evaluate to a boolean"),
            "got: {error}"
        );
    }

    #[test]
    fn an_invalid_expression_fails_to_compile() {
        assert!(CompiledFilter::new("x >").is_err());
        assert!(CompiledFilter::new("((").is_err());
    }

    #[test]
    fn referenced_paths_split_payload_from_metadata() {
        let filter = CompiledFilter::new("order.status == 'x' and meta.kind == 'y'").unwrap();
        assert_eq!(
            filter.payload_paths,
            vec![vec!["order".to_string(), "status".to_string()]]
        );
        assert_eq!(filter.metadata_keys, vec!["kind".to_string()]);
    }

    #[test]
    fn missing_fields_are_null_so_other_boolean_branches_can_match() {
        let filter = CompiledFilter::new("missing == null or x == 1").unwrap();
        assert!(filter.matches(&message(r#"{"x": 1}"#, &[])).unwrap());

        let filter = CompiledFilter::new("meta.missing == null or x == 1").unwrap();
        assert!(filter.matches(&message(r#"{"x": 1}"#, &[])).unwrap());
    }

    #[test]
    fn indexed_payload_paths_are_rejected_at_compile_time() {
        let error = CompiledFilter::new("items[0].qty == 1")
            .err()
            .expect("an indexed path must not compile")
            .to_string();
        assert!(error.contains("indexed path"), "unexpected error: {error}");
    }

    /// Every predicate the fast route now claims must answer exactly as the expression
    /// engine does — including where the engine raises an error, which the fast route has
    /// to defer rather than silently turn into `false`.
    #[test]
    fn fast_predicates_match_zen_across_operators_and_readings() {
        let expressions = [
            "amount == 100",
            "amount != 100",
            "amount > 100",
            "amount >= 100",
            "amount < 100",
            "amount <= 100",
            "100 < amount",
            "100 >= amount",
            "amount > 100 and country == 'US'",
            "amount > 100 or country == 'US'",
            "country == 'US' or amount > 100 and active == true",
            "country == 'US' and amount > 100 or active == true",
            "a == 1 and b == 2 and c == 3",
            "a == 1 or b == 2 or c == 3",
            "meta.retries > 3",
            "meta.kind == 'order' and amount > 100",
            "amount == null",
            "amount != null",
        ];

        let payloads = [
            r#"{"amount":100,"country":"US","active":true,"a":1,"b":2,"c":3}"#,
            r#"{"amount":101,"country":"DE","active":false,"a":1,"b":9,"c":3}"#,
            // Text-typed columns, as a CSV or SQL source delivers them.
            r#"{"amount":"100","country":"US","active":true}"#,
            r#"{"amount":"250.75","country":"US"}"#,
            // Readings the engine refuses: text that is not a number, a bool, a
            // container, an explicit null, and an absent field.
            r#"{"amount":"abc","country":"US"}"#,
            r#"{"amount":true,"country":"US"}"#,
            r#"{"amount":[1,2],"country":"US"}"#,
            r#"{"amount":{"v":1},"country":"US"}"#,
            r#"{"amount":null,"country":"US"}"#,
            r#"{"country":"US"}"#,
            r#"{}"#,
            // Duplicate keys must resolve the way a `Value` parse would: last wins.
            r#"{"amount":1,"amount":900}"#,
        ];

        let metadata_sets: [&[(&str, &str)]; 3] = [
            &[],
            &[("retries", "5"), ("kind", "order")],
            &[("retries", "x")],
        ];

        for expression in expressions {
            let fast = CompiledFilter::new(expression).unwrap();
            assert!(
                fast.fast_predicate.is_some(),
                "{expression} should compile to the fast path"
            );
            let mut zen = CompiledFilter::new(expression).unwrap();
            zen.fast_predicate = None;

            for payload in payloads {
                for metadata in metadata_sets {
                    let message = message(payload, metadata);
                    let (fast, zen) = (fast.matches(&message), zen.matches(&message));
                    match (fast, zen) {
                        (Ok(fast), Ok(zen)) => assert_eq!(
                            fast, zen,
                            "{expression} disagreed on {payload} with {metadata:?}"
                        ),
                        (Err(_), Err(_)) => {}
                        (fast, zen) => panic!(
                            "{expression} on {payload} with {metadata:?}: \
                             fast={fast:?} zen={zen:?}"
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn comparison_and_boolean_predicates_compile_to_the_fast_path() {
        for expression in [
            "x == 1",
            "1 == x",
            "x != null",
            "enabled == true",
            "order.status == 'open'",
            "meta.kind != 'ignored'",
            "x > 1",
            "1 < x",
            "x >= 1",
            "x <= 1",
            "x == 1 or y == 2",
            "x > 1 and y < 2",
            "a == 1 and b == 2 and c == 3",
            "a == 1 or b > 2 and c == 3",
            "meta.retries > 3 and status == 'open'",
        ] {
            assert!(
                CompiledFilter::new(expression)
                    .unwrap()
                    .fast_predicate
                    .is_some(),
                "{expression}"
            );
        }

        for expression in [
            // A call or arithmetic is not a bare field against a bare literal.
            "number(meta.count) >= 2",
            "x + 1 == 2",
            // The engine orders numbers only, so ordering against anything else has to
            // reach the slow path that reports it as a typed-field problem.
            "x > 'a'",
            "x >= true",
            // Neither side is a literal.
            "x == y",
        ] {
            assert!(
                CompiledFilter::new(expression)
                    .unwrap()
                    .fast_predicate
                    .is_none(),
                "{expression}"
            );
        }
    }

    #[test]
    fn fast_equality_matches_zen_across_values_and_missing_fields() {
        let cases = [
            ("x == 1", r#"{"x":1}"#, &[][..]),
            ("x == 1", r#"{"x":1.0}"#, &[][..]),
            ("x == 1", r#"{"x":"1"}"#, &[][..]),
            ("x != 1", r#"{}"#, &[][..]),
            ("x == null", r#"{}"#, &[][..]),
            ("enabled == true", r#"{"enabled":true}"#, &[][..]),
            (
                "order.status == 'open'",
                r#"{"order":{"status":"open"}}"#,
                &[][..],
            ),
            ("meta.kind == 'order'", "not json", &[("kind", "order")][..]),
            ("meta.kind != 'order'", "not json", &[][..]),
        ];

        for (expression, payload, metadata) in cases {
            let fast = CompiledFilter::new(expression).unwrap();
            assert!(fast.fast_predicate.is_some(), "{expression}");
            let mut zen = CompiledFilter::new(expression).unwrap();
            zen.fast_predicate = None;
            let message = message(payload, metadata);
            assert_eq!(
                fast.matches(&message).unwrap(),
                zen.matches(&message).unwrap(),
                "{expression} with {payload}"
            );
        }
    }

    /// The documented rule: `meta` is the metadata namespace, so a payload field of
    /// the same name must not decide the predicate.
    #[test]
    fn message_metadata_shadows_a_payload_field_named_meta() {
        let filter = CompiledFilter::new("x > 1 and meta.kind == 'real'").unwrap();
        let payload = r#"{"x": 2, "meta": {"kind": "payload"}}"#;
        assert!(filter
            .matches(&message(payload, &[("kind", "real")]))
            .unwrap());
        assert!(!filter
            .matches(&message(payload, &[("kind", "other")]))
            .unwrap());
    }

    /// A `switch` runs every `when` case against one context, so the nulls one
    /// predicate synthesizes for an absent field must not change the next one's answer.
    #[test]
    fn a_synthesized_null_does_not_leak_into_the_next_predicate() {
        let first = CompiledFilter::new("a.b == 1").unwrap();
        let mut second = CompiledFilter::new("a == null or x == 1").unwrap();
        second.fast_predicate = None;
        let message = message(r#"{"x": 2}"#, &[]);

        let alone = second
            .matches_with_context(&message, &mut FilterContext::new())
            .unwrap();

        let mut shared = FilterContext::new();
        assert!(!first.matches_with_context(&message, &mut shared).unwrap());
        assert_eq!(
            second.matches_with_context(&message, &mut shared).unwrap(),
            alone,
            "predicate order changed the answer"
        );
    }

    #[test]
    fn top_level_fast_equality_does_not_build_a_json_document() {
        let filter = CompiledFilter::new("wanted == 'yes'").unwrap();
        let mut context = FilterContext::new();
        let ignored = "x".repeat(10_000);
        let payload = format!(r#"{{"ignored":"{ignored}","wanted":"yes"}}"#);

        assert!(filter
            .matches_with_context(&message(&payload, &[]), &mut context)
            .unwrap());
        assert!(!context.payload_loaded);

        let metadata = CompiledFilter::new("meta.kind == 'order'").unwrap();
        assert!(metadata
            .matches_with_context(
                &message("not json", &[("kind", "order")]),
                &mut FilterContext::new(),
            )
            .unwrap());
    }

    /// A source whose commits must stay in order, handing out one prepared batch per read
    /// and an empty batch once they run out.
    struct OrderedSource {
        batches: std::collections::VecDeque<Vec<CanonicalMessage>>,
        committed: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    }

    impl OrderedSource {
        fn new(
            batches: Vec<Vec<CanonicalMessage>>,
        ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<usize>>>) {
            let committed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let source = Self {
                batches: batches.into(),
                committed: committed.clone(),
            };
            (source, committed)
        }
    }

    #[async_trait]
    impl MessageConsumer for OrderedSource {
        async fn receive(&mut self) -> Result<Received, ConsumerError> {
            unimplemented!("batch-only test source")
        }

        async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
            let messages = self.batches.pop_front().unwrap_or_default();
            let committed = self.committed.clone();
            Ok(ReceivedBatch {
                messages,
                commit: Box::new(move |dispositions| {
                    Box::pin(async move {
                        committed.lock().unwrap().push(dispositions.len());
                        Ok(())
                    })
                }),
            })
        }

        fn commit_requires_order(&self) -> bool {
            true
        }

        async fn close(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn amount(value: i64) -> CanonicalMessage {
        message(&format!(r#"{{"amount":{value}}}"#), &[])
    }

    /// An emptied batch must not ack ahead of the route's ordered sequencer, so its commit
    /// is held and runs from inside the next retained batch's.
    #[tokio::test]
    async fn an_emptied_batch_is_acked_in_front_of_the_batch_that_followed_it() {
        let (source, committed) =
            OrderedSource::new(vec![vec![amount(1), amount(2)], vec![amount(500)]]);
        let mut consumer = FilterConsumer::new(Box::new(source), "amount > 100").unwrap();

        let batch = consumer.receive_batch(16).await.unwrap();
        assert_eq!(
            batch.messages.len(),
            1,
            "only the matching message survives"
        );
        assert!(
            committed.lock().unwrap().is_empty(),
            "the emptied batch must not ack ahead of the route"
        );

        (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        assert_eq!(
            committed.lock().unwrap().as_slice(),
            [2, 1],
            "the emptied batch is acked first, then the retained one"
        );
    }

    #[tokio::test]
    async fn filtered_full_source_batches_are_refilled_to_the_requested_size() {
        let (source, committed) = OrderedSource::new(vec![
            vec![amount(1), amount(200), amount(2), amount(300)],
            vec![amount(400), amount(3), amount(500), amount(4)],
        ]);
        let mut consumer = FilterConsumer::new(Box::new(source), "amount > 100").unwrap();

        let batch = consumer.receive_batch(4).await.unwrap();
        assert_eq!(
            batch
                .messages
                .iter()
                .map(CanonicalMessage::get_payload_str)
                .collect::<Vec<_>>(),
            [
                r#"{"amount":200}"#,
                r#"{"amount":300}"#,
                r#"{"amount":400}"#,
                r#"{"amount":500}"#,
            ]
        );

        (batch.commit)(vec![MessageDisposition::Ack; 4])
            .await
            .unwrap();
        assert_eq!(
            committed.lock().unwrap().as_slice(),
            [4, 4],
            "source batch commits stay in source order"
        );
    }

    #[tokio::test]
    async fn merged_filter_commit_validates_count_before_committing_any_source_batch() {
        let (source, committed) = OrderedSource::new(vec![
            vec![amount(200), amount(1), amount(300), amount(2)],
            vec![amount(400), amount(3), amount(500), amount(4)],
        ]);
        let mut consumer = FilterConsumer::new(Box::new(source), "amount > 100").unwrap();

        let batch = consumer.receive_batch(4).await.unwrap();
        let error = (batch.commit)(vec![MessageDisposition::Ack; 3])
            .await
            .unwrap_err();

        assert!(error.to_string().contains("3 dispositions for 4 retained"));
        assert!(
            committed.lock().unwrap().is_empty(),
            "an invalid merged commit must not partially commit its first source batch"
        );
    }

    /// Nothing follows a drain to carry the held commit, so the drain itself must flush it —
    /// otherwise a route whose tail matches nothing never advances its source position.
    #[tokio::test]
    async fn a_final_emptied_batch_is_acked_when_the_source_drains() {
        let (source, committed) = OrderedSource::new(vec![vec![amount(1), amount(2)]]);
        let mut consumer = FilterConsumer::new(Box::new(source), "amount > 100").unwrap();

        let drained = consumer.receive_batch(16).await.unwrap();
        assert!(drained.messages.is_empty(), "the source is drained");
        assert_eq!(
            committed.lock().unwrap().as_slice(),
            [2],
            "the dropped-only final batch is acknowledged before drain returns"
        );
    }

    /// A route torn down without reading past the emptied batch still releases its commit.
    #[tokio::test]
    async fn a_held_commit_is_released_on_disconnect() {
        let (source, committed) = OrderedSource::new(vec![vec![amount(1)], vec![amount(2)]]);
        let mut consumer = FilterConsumer::new(Box::new(source), "amount > 100").unwrap();

        let batch = consumer.receive_batch(16).await.unwrap();
        assert!(
            batch.messages.is_empty(),
            "both batches are dropped, then drain"
        );
        assert_eq!(committed.lock().unwrap().len(), 2, "flushed by the drain");

        let (source, committed) = OrderedSource::new(vec![vec![amount(1)]]);
        let mut consumer = FilterConsumer::new(Box::new(source), "amount > 100").unwrap();
        // Stop after the emptied batch, before the read that would drain the source.
        let batch = consumer.inner.receive_batch(16).await.unwrap();
        consumer
            .deferred
            .ack_emptied(true, batch.commit, batch.messages.len())
            .await
            .unwrap();
        assert!(committed.lock().unwrap().is_empty());

        consumer.on_disconnect_hook().unwrap().await.unwrap();
        assert_eq!(committed.lock().unwrap().as_slice(), [1]);
    }

    /// The route runs the hooks of the outermost publisher only, so a filter that did not
    /// delegate them would silently disable an endpoint's connect and teardown — including
    /// the structural endpoints that reach their nested destinations that way.
    #[tokio::test]
    async fn publisher_lifecycle_is_delegated_to_the_wrapped_sink() {
        #[derive(Default)]
        struct HookedSink {
            connected: Arc<AtomicBool>,
            disconnected: Arc<AtomicBool>,
            flushed: Arc<AtomicBool>,
        }

        #[async_trait]
        impl MessagePublisher for HookedSink {
            fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
                Some(Box::pin(async move {
                    self.connected.store(true, Ordering::Relaxed);
                    Ok(())
                }))
            }

            fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
                Some(Box::pin(async move {
                    self.disconnected.store(true, Ordering::Relaxed);
                    Ok(())
                }))
            }

            async fn flush(&self) -> anyhow::Result<()> {
                self.flushed.store(true, Ordering::Relaxed);
                Ok(())
            }

            async fn send_batch(
                &self,
                _messages: Vec<CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                Ok(SentBatch::Ack)
            }

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let sink = HookedSink::default();
        let (connected, disconnected, flushed) = (
            sink.connected.clone(),
            sink.disconnected.clone(),
            sink.flushed.clone(),
        );
        let publisher = FilterPublisher::new(Box::new(sink), "amount > 100").unwrap();

        publisher.on_connect_hook().unwrap().await.unwrap();
        publisher.flush().await.unwrap();
        publisher.on_disconnect_hook().unwrap().await.unwrap();

        assert!(
            connected.load(Ordering::Relaxed),
            "connect hook reached the sink"
        );
        assert!(
            disconnected.load(Ordering::Relaxed),
            "disconnect hook reached the sink"
        );
        assert!(flushed.load(Ordering::Relaxed), "flush reached the sink");
    }
}
