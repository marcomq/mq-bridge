//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Turning bytes into records and records into bytes, independent of where they are stored.

pub(crate) mod csv;
mod csv_dialect;
pub(crate) mod format;
pub(crate) mod framing;

pub(crate) use csv_dialect::{CsvDialect, CsvSyntax};
