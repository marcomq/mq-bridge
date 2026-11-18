//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The CSV dialect a `file` or `object_store` endpoint reads and writes.

use crate::errors::InvalidConfig;
use crate::models::{CsvConfig, CsvMismatch, CsvNested, FileFormat};
use anyhow::{anyhow, bail};
use std::sync::{Arc, OnceLock};

/// What `separator: auto` chooses between, in order of preference on a tie.
const AUTO_CANDIDATES: [u8; 4] = *b",;\t|";

/// The bytes that carry CSV syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CsvSyntax {
    pub(crate) separator: u8,
    /// `None` reads and writes every field unquoted.
    pub(crate) quote: Option<u8>,
}

impl Default for CsvSyntax {
    fn default() -> Self {
        Self {
            separator: b',',
            quote: Some(b'"'),
        }
    }
}

#[derive(Debug, Clone)]
enum Separator {
    Fixed(u8),
    /// Guessed from the first record; shared so every reader of one source agrees.
    Auto(Arc<OnceLock<u8>>),
}

#[derive(Debug, Clone)]
pub(crate) struct CsvDialect {
    separator: Separator,
    pub(crate) quote: Option<u8>,
    /// Whether the first record names the columns.
    pub(crate) header: bool,
    /// Column names from the config; they take the place of the header's.
    pub(crate) columns: Option<Arc<[String]>>,
    /// (Sink) Nested objects become `parent.child` columns instead of JSON text.
    pub(crate) flatten: bool,
    /// (Sink) A record whose keys differ from the columns fails instead of being written.
    pub(crate) strict: bool,
}

impl Default for CsvDialect {
    fn default() -> Self {
        Self {
            separator: Separator::Fixed(b','),
            quote: Some(b'"'),
            header: true,
            columns: None,
            flatten: true,
            strict: false,
        }
    }
}

impl CsvDialect {
    /// The dialect of an endpoint; the default one when `format` is not CSV.
    pub(crate) fn for_format(
        format: &FileFormat,
        config: &CsvConfig,
        delimiter: &[u8],
    ) -> anyhow::Result<Self> {
        match format {
            FileFormat::Csv => {
                Self::from_config(config, delimiter).map_err(|e| InvalidConfig(e).into())
            }
            _ => {
                if *config != CsvConfig::default() {
                    tracing::warn!("`csv` settings are ignored: they apply to `format: csv` only");
                }
                Ok(Self::default())
            }
        }
    }

    /// `delimiter` is the record separator, which the field syntax must stay clear of.
    pub(crate) fn from_config(config: &CsvConfig, delimiter: &[u8]) -> anyhow::Result<Self> {
        let quote = match config.quote.as_deref() {
            Some("none") => None,
            Some(value) => Some(parse_byte(value, "quote")?),
            None => Some(b'"'),
        };
        let (separator, separators): (_, &[u8]) = match config.separator.as_deref() {
            Some("auto") => (Separator::Auto(Arc::new(OnceLock::new())), &AUTO_CANDIDATES),
            Some(value) => (Separator::Fixed(parse_byte(value, "separator")?), &[]),
            None => (Separator::Fixed(b','), &[]),
        };
        let fixed = match separator {
            Separator::Fixed(byte) => Some(byte),
            Separator::Auto(_) => None,
        };
        let mut syntax = separators.iter().copied().chain(fixed).chain(quote);
        if quote.is_some_and(|q| fixed == Some(q) || separators.contains(&q)) {
            bail!("csv: `separator` and `quote` must differ");
        }
        if syntax.any(|byte| delimiter.contains(&byte)) {
            bail!("csv: the record `delimiter` must not contain the `separator` or `quote`");
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(name) = config.columns.iter().find(|name| !seen.insert(*name)) {
            bail!("csv: `columns` repeats '{name}'");
        }
        Ok(Self {
            separator,
            quote,
            header: config.header.unwrap_or(true),
            columns: (!config.columns.is_empty()).then(|| config.columns.as_slice().into()),
            flatten: config.nested == CsvNested::Flatten,
            strict: config.on_mismatch == CsvMismatch::Fail,
        })
    }

    /// A source has nothing else to name its columns by.
    pub(crate) fn check_source(&self) -> anyhow::Result<()> {
        if !self.header && self.columns.is_none() {
            return Err(InvalidConfig(anyhow!(
                "csv: a source with `header: false` needs `columns`"
            ))
            .into());
        }
        if self.strict {
            return Err(InvalidConfig(anyhow!(
                "csv: `on_mismatch: fail` is for sinks; a source reads short and long rows as they are"
            ))
            .into());
        }
        Ok(())
    }

    /// A sink has no record to guess the separator from.
    pub(crate) fn check_sink(&self) -> anyhow::Result<()> {
        if matches!(self.separator, Separator::Auto(_)) {
            return Err(InvalidConfig(anyhow!(
                "csv: `separator: auto` is for sources; a sink needs the separator itself"
            ))
            .into());
        }
        Ok(())
    }

    /// Whether a sink appending to a file takes its columns from the header already there.
    #[cfg_attr(
        not(any(feature = "compression", feature = "encryption")),
        allow(dead_code)
    )]
    pub(crate) fn reads_header_back(&self) -> bool {
        self.header && self.columns.is_none()
    }

    /// The syntax to read or write with. An `auto` separator still undecided reads as `,`.
    pub(crate) fn syntax(&self) -> CsvSyntax {
        let separator = match &self.separator {
            Separator::Fixed(byte) => *byte,
            Separator::Auto(found) => found.get().copied().unwrap_or(b','),
        };
        CsvSyntax {
            separator,
            quote: self.quote,
        }
    }

    /// [`Self::syntax`], settling an `auto` separator on `first_record` if it is still open.
    pub(crate) fn resolve(&self, first_record: &[u8]) -> CsvSyntax {
        if let Separator::Auto(found) = &self.separator {
            found.get_or_init(|| {
                let separator = guess_separator(first_record, self.quote);
                tracing::info!(separator = %(separator as char).escape_default(), "CSV separator detected");
                separator
            });
        }
        self.syntax()
    }
}

/// The candidate that occurs most often outside quotes; `,` when none does.
fn guess_separator(record: &[u8], quote: Option<u8>) -> u8 {
    let mut counts = [0usize; AUTO_CANDIDATES.len()];
    let mut in_quotes = false;
    // A quote opens a section at the start of a field, or right after one closed (`""`).
    let mut may_open = true;
    for &byte in record {
        if Some(byte) == quote {
            // A quote inside an unquoted field is literal and opens nothing after it.
            (in_quotes, may_open) = if in_quotes {
                (false, true)
            } else {
                (may_open, false)
            };
        } else if !in_quotes {
            may_open = AUTO_CANDIDATES
                .iter()
                .position(|&c| c == byte)
                .is_some_and(|i| {
                    counts[i] += 1;
                    true
                });
        }
    }
    let mut best = 0;
    for (i, &count) in counts.iter().enumerate() {
        if count > counts[best] {
            best = i;
        }
    }
    AUTO_CANDIDATES[best]
}

/// One ASCII byte: a character, a name (`tab`, `space`, …) or hex like `0x1f`.
fn parse_byte(value: &str, what: &str) -> anyhow::Result<u8> {
    let invalid = || {
        anyhow!("csv: `{what}` must be one ASCII character, `tab`, `space` or hex like 0x1f, got '{value}'")
    };
    let byte = match value {
        "tab" | "\\t" => b'\t',
        "space" => b' ',
        "comma" => b',',
        "semicolon" => b';',
        "pipe" => b'|',
        hex if hex.len() == 4 && hex.starts_with("0x") => {
            u8::from_str_radix(&hex[2..], 16).map_err(|_| invalid())?
        }
        one if one.len() == 1 => one.as_bytes()[0],
        _ => return Err(invalid()),
    };
    if !byte.is_ascii() || matches!(byte, b'\n' | b'\r') {
        return Err(invalid());
    }
    Ok(byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialect(separator: &str, quote: Option<&str>) -> anyhow::Result<CsvDialect> {
        let config = CsvConfig {
            separator: Some(separator.to_string()),
            quote: quote.map(str::to_string),
            ..Default::default()
        };
        CsvDialect::from_config(&config, b"\n")
    }

    #[test]
    fn separator_takes_characters_names_and_hex() {
        for (spelling, byte) in [
            (";", b';'),
            ("tab", b'\t'),
            ("\\t", b'\t'),
            ("\t", b'\t'),
            ("space", b' '),
            ("pipe", b'|'),
            ("0x1f", 0x1f),
        ] {
            assert_eq!(
                dialect(spelling, None).unwrap().syntax().separator,
                byte,
                "{spelling}"
            );
        }
        assert_eq!(dialect("tab", Some("none")).unwrap().quote, None);
        assert_eq!(dialect(",", Some("'")).unwrap().quote, Some(b'\''));
    }

    #[test]
    fn rejects_what_the_parser_cannot_tell_apart() {
        assert!(dialect(";;", None).is_err());
        assert!(dialect("ä", None).is_err());
        assert!(dialect("0x0a", None).is_err());
        assert!(dialect("\"", None).is_err());
        let csv = CsvConfig {
            separator: Some("|".to_string()),
            ..Default::default()
        };
        assert!(CsvDialect::from_config(&csv, b"|\n").is_err());
        let csv = CsvConfig {
            columns: vec!["a".to_string(), "a".to_string()],
            ..Default::default()
        };
        assert!(CsvDialect::from_config(&csv, b"\n").is_err());
    }

    #[test]
    fn a_source_without_a_header_needs_columns() {
        let mut csv = CsvConfig {
            header: Some(false),
            ..Default::default()
        };
        let headless = CsvDialect::from_config(&csv, b"\n").unwrap();
        assert!(headless.check_source().is_err());
        csv.columns = vec!["id".to_string()];
        assert!(CsvDialect::from_config(&csv, b"\n")
            .unwrap()
            .check_source()
            .is_ok());
        csv.on_mismatch = CsvMismatch::Fail;
        let strict = CsvDialect::from_config(&csv, b"\n").unwrap();
        assert!(strict.check_source().is_err());
        assert!(strict.check_sink().is_ok());
    }

    #[test]
    fn auto_picks_the_most_frequent_candidate_outside_quotes() {
        for (first_record, separator) in [
            ("id;name;city\r\n", b';'),
            ("id\tname\tcity\n", b'\t'),
            ("id|name\n", b'|'),
            ("\"a;b;c\",d,e\n", b','),
            ("a\"b;c;d\n", b';'),
            ("a\"\"b;c;d\n", b';'),
            ("\"a \"\" , ,\";b;c\n", b';'),
            ("single\n", b','),
        ] {
            let auto = dialect("auto", None).unwrap();
            assert!(auto.check_sink().is_err());
            assert_eq!(
                auto.resolve(first_record.as_bytes()).separator,
                separator,
                "{first_record:?}"
            );
            // Settled: a later record cannot change it.
            assert_eq!(auto.resolve(b"a,b,c,d,e,f").separator, separator);
        }
    }
}
