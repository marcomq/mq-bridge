//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Cutting bytes into records on a delimiter. For CSV a delimiter inside a quoted field is
//! data, so a record can span several delimiters.

use super::csv::CsvQuoteState;
use super::CsvDialect;
use crate::models::FileFormat;

/// Quote state after `first`, a record's first delimited piece; `None` unless the format
/// quotes. A streaming reader reads on while the state is in quotes.
pub(crate) fn record_quotes(
    format: &FileFormat,
    csv: &CsvDialect,
    first: &[u8],
) -> Option<CsvQuoteState> {
    matches!(format, FileFormat::Csv).then(|| {
        let mut quotes = CsvQuoteState::new(csv.resolve(first));
        quotes.feed(first);
        quotes
    })
}

/// Splits a buffer into record spans on `delimiter`, dropping a trailing empty
/// remainder and a stray `\r` before a `\n` delimiter (mirrors the file reader). With
/// `csv`, a delimiter inside a quoted field is data, as it is for the file reader.
#[cfg(feature = "object-store")]
pub(crate) fn split_records(
    data: &[u8],
    delimiter: &[u8],
    csv: Option<&CsvDialect>,
) -> Vec<super::format::RecordSpan> {
    let mut records = Vec::new();
    if delimiter.is_empty() {
        return records;
    }
    let newline = delimiter == b"\n";
    let finder = memchr::memmem::Finder::new(delimiter);
    // An `auto` separator is settled on the buffer's first line.
    let syntax = csv.map(|csv| {
        let first_line = finder.find(data).unwrap_or(data.len());
        csv.resolve(&data[..first_line])
    });
    let mut quotes = CsvQuoteState::new(syntax.unwrap_or_default());
    let (mut scanned, mut start, mut from) = (0, 0, 0);
    while let Some(at) = finder.find(&data[from..]).map(|at| from + at) {
        if let Some(syntax) = syntax {
            quotes.feed(&data[scanned..at]);
            scanned = at;
            if quotes.in_quotes() {
                from = at + 1;
                continue;
            }
            quotes = CsvQuoteState::new(syntax);
            scanned = at + delimiter.len();
        }
        let end = at - usize::from(newline && at > start && data[at - 1] == b'\r');
        records.push((start, end, start as u64));
        start = at + delimiter.len();
        from = start;
    }
    if start < data.len() {
        records.push((start, data.len(), start as u64));
    }
    records
}
