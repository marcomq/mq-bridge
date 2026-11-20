use super::*;
use crate::codec::csv::CsvHeader;

/// The reader decodes a batch across cores; that must be invisible. At every size around
/// the split threshold the parallel decode has to match a plain sequential one, record
/// for record, including the header row it swallows and the offsets it stamps.
#[test]
fn parallel_record_decode_matches_a_sequential_one() {
    let header = b"id,name,amount,note".to_vec();
    let row = |i: usize| format!(r#"{i},"a,b {i}",{i}.5,"say ""hi"" {i}""#).into_bytes();

    for count in [0, 1, 2, 63, 64, 65, 127, 1024] {
        for with_header in [true, false] {
            let mut buf: Vec<u8> = Vec::new();
            let mut spans: Vec<RecordSpan> = Vec::new();
            let mut records: Vec<Vec<u8>> = Vec::new();
            if with_header {
                records.push(header.clone());
            }
            records.extend((0..count).map(row));
            for (i, record) in records.iter().enumerate() {
                let start = buf.len();
                buf.extend_from_slice(record);
                spans.push((start, buf.len(), i as u64 + 1));
            }

            let mut state = (!with_header).then(|| Arc::new(CsvHeader::parse(&header)));
            let actual = decode_records(&mut buf, &spans, &FileFormat::Csv, &mut state, true);
            // The batch buffer is lent to the workers and must come back intact, or the
            // reader silently reallocates it every batch.
            assert_eq!(
                buf.len(),
                records.iter().map(Vec::len).sum::<usize>(),
                "{count} records, header={with_header}: buffer not handed back"
            );

            let mut expected_state = (!with_header).then(|| CsvHeader::parse(&header));
            let expected: Vec<_> = spans
                .iter()
                .filter_map(|&(start, end, position)| {
                    let mut msg =
                        parse_message(&buf[start..end], &FileFormat::Csv, &mut expected_state)?;
                    msg.set_metadata("file_offset", position.to_string());
                    Some(msg)
                })
                .collect();

            assert_eq!(
                actual.len(),
                expected.len(),
                "{count} records, header={with_header}: wrong count"
            );
            for (got, want) in actual.iter().zip(&expected) {
                assert_eq!(got.payload, want.payload, "payload differs");
                assert_eq!(
                    got.get_metadata("file_offset"),
                    want.get_metadata("file_offset"),
                    "offset differs"
                );
            }
        }
    }
}
