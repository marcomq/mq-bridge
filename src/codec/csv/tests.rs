use super::*;

#[test]
fn test_csv_ends_inside_quotes_tracks_field_starts() {
    assert!(csv_ends_inside_quotes(b"3,\"Line1\n"));
    assert!(!csv_ends_inside_quotes(b"3,\"Line1\nLine2\",\n"));
    // Doubled quotes are an escape, not a close.
    assert!(csv_ends_inside_quotes(b"1,\"a \"\"b\n"));
    assert!(!csv_ends_inside_quotes(b"1,\"a \"\"b\"\n"));
    // A quote that does not start a field is literal data, matching `parse_csv_row`.
    assert!(!csv_ends_inside_quotes(b"1,in\"ch\n"));
    // A leading BOM does not stop the quote after it from opening the field.
    assert!(csv_ends_inside_quotes(b"\xef\xbb\xbf\"i\n"));
}

/// The quote scan jumps between quotes; it must land in the same state as the plain
/// byte-at-a-time walk, wherever the input is split.
#[test]
fn test_csv_quote_state_matches_bytewise_walk() {
    // (in_quotes, field_is_empty, pending_quote) after a byte-wise walk.
    fn walk(state: (bool, bool, bool), bytes: &[u8]) -> (bool, bool, bool) {
        let (mut in_quotes, mut field_is_empty, mut pending_quote) = state;
        for &b in bytes {
            if pending_quote {
                pending_quote = false;
                if b == b'"' {
                    continue;
                }
                in_quotes = false;
            }
            if in_quotes {
                pending_quote = b == b'"';
            } else if b == b'"' && field_is_empty {
                in_quotes = true;
            } else {
                field_is_empty = b == b',';
            }
        }
        (in_quotes, field_is_empty, pending_quote)
    }

    // Short tokens exercise every transition; the long run crosses the scalar probe.
    let tokens: [&[u8]; 5] = [b"\"", b",", b"a", b"\n", &[b'x'; 20]];
    let mut inputs: Vec<Vec<u8>> = vec![Vec::new()];
    for _ in 0..6 {
        inputs = inputs
            .iter()
            .flat_map(|prefix| tokens.iter().map(move |t| [prefix.as_slice(), t].concat()))
            .collect();
        for input in &inputs {
            for split in 0..=input.len() {
                let (head, tail) = input.split_at(split);
                let mut state = CsvQuoteState::default();
                state.feed(head);
                state.feed(tail);
                let expected = walk(walk((false, true, false), head), tail);
                assert_eq!(
                    (state.in_quotes, state.field_is_empty, state.pending_quote),
                    expected,
                    "input {:?} split at {split}",
                    String::from_utf8_lossy(input)
                );
            }
        }
    }
}

/// Quoting is decided per field: exactly the characters that would change how the
/// reader frames or splits the record trigger it, and nothing else is touched.
#[test]
fn csv_append_field_quotes_exactly_when_needed() {
    let cases: &[(&str, &[u8], &str)] = &[
        ("plain", b"\n", "plain"),
        ("", b"\n", ""),
        ("  padded  ", b"\n", "  padded  "),
        ("tab\there", b"\n", "tab\there"),
        ("héllo 世界 🎉", b"\n", "héllo 世界 🎉"),
        ("back\\slash", b"\n", "back\\slash"),
        ("a,b", b"\n", "\"a,b\""),
        ("\"", b"\n", "\"\"\"\""),
        ("say \"hi\"", b"\n", "\"say \"\"hi\"\"\""),
        ("a\nb", b"\n", "\"a\nb\""),
        ("a\rb", b"\n", "\"a\rb\""),
        ("a\r\nb", b"\r\n", "\"a\r\nb\""),
        ("trailing\r", b"\n", "\"trailing\r\""),
        // A custom delimiter only matters when it is the one in use.
        ("a|b", b"\n", "a|b"),
        ("a|b", b"|", "\"a|b\""),
        ("a;b", b";;", "a;b"),
        ("a;;b", b";;", "\"a;;b\""),
        // A tail that starts the delimiter would complete it one byte early.
        (";", b";;", "\";\""),
        ("ab", b"bc", "\"ab\""),
        ("ba", b"bc", "ba"),
        // A leading U+FEFF would be read back as a byte-order mark; elsewhere it is data.
        ("\u{feff}id", b"\n", "\"\u{feff}id\""),
        ("id\u{feff}", b"\n", "id\u{feff}"),
    ];
    for (input, delimiter, expected) in cases {
        let mut buf = b"prefix".to_vec();
        csv_append_field(&mut buf, input, delimiter, Default::default()).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!("prefix{expected}"),
            "field {input:?} with delimiter {:?}",
            String::from_utf8_lossy(delimiter)
        );
    }
}

/// Every byte class the escaper distinguishes must produce a JSON string that decodes
/// back to the input, with no raw control character left in the output.
#[test]
fn json_append_escaped_output_is_valid_json_for_every_byte_class() {
    let mut inputs: Vec<String> = (0u8..0x20).map(|b| char::from(b).to_string()).collect();
    inputs.extend(
        [
            "\"",
            "\\",
            "\u{7f}",
            "\u{2028}\u{2029}",
            "é",
            "世",
            "🎉",
            "",
            "plain",
            "mixed \"q\" \\ \n\t\u{1} é世🎉 \u{7f} end",
            "\\\"\\\"",
        ]
        .map(String::from),
    );
    for input in &inputs {
        let mut buf = b"\"".to_vec();
        json_append_escaped(&mut buf, input.as_bytes());
        buf.push(b'"');
        assert!(
            buf.iter().all(|&b| b >= 0x20),
            "raw control byte left in {:?}",
            String::from_utf8_lossy(&buf)
        );
        assert_eq!(
            &serde_json::from_slice::<String>(&buf).unwrap(),
            input,
            "escaped as {:?}",
            String::from_utf8_lossy(&buf)
        );
    }

    let spelled = |s: &str| {
        let mut buf = Vec::new();
        json_append_escaped(&mut buf, s.as_bytes());
        String::from_utf8(buf).unwrap()
    };
    assert_eq!(spelled("\u{8}\u{c}\n\r\t"), r"\b\f\n\r\t");
    assert_eq!(spelled("\u{0}\u{1f}"), r"\u0000\u001f");
    assert_eq!(spelled("\u{7f}\u{2028}"), "\u{7f}\u{2028}");
}
