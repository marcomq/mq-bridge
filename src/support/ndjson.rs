//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Frames JSON payloads as NDJSON request bodies, for sinks that take documents
//! in bulk over HTTP and cap the size of one request.

use anyhow::{Context, anyhow};

/// Appends one JSON object as an NDJSON line. The bytes are copied through
/// untouched unless the payload spans several lines, which would break the framing.
pub fn append_line(body: &mut Vec<u8>, payload: &[u8]) -> anyhow::Result<()> {
    let payload = payload.trim_ascii();
    if payload.first() != Some(&b'{') {
        return Err(anyhow!("the payload is not a JSON object"));
    }
    if payload.contains(&b'\n') || payload.contains(&b'\r') {
        let document: serde_json::Value =
            serde_json::from_slice(payload).context("the payload is not valid JSON")?;
        serde_json::to_writer(&mut *body, &document)
            .context("failed to re-serialize a multi-line JSON payload")?;
    } else {
        serde_json::from_slice::<serde::de::IgnoredAny>(payload)
            .context("the payload is not valid JSON")?;
        body.extend_from_slice(payload);
    }
    body.push(b'\n');
    Ok(())
}

/// Frames `payloads` as NDJSON bodies in order, starting a new body whenever the
/// current one would grow past `max_bytes`. A single payload over the limit is
/// still returned, alone: it cannot be split, and the sink names the limit best.
pub fn chunk<'a>(
    payloads: impl IntoIterator<Item = &'a [u8]>,
    max_bytes: usize,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let mut body = Vec::new();
    for payload in payloads {
        let mark = body.len();
        append_line(&mut body, payload)?;
        if mark > 0 && body.len() > max_bytes {
            let carried = body.split_off(mark);
            bodies.push(std::mem::replace(&mut body, carried));
        }
    }
    if !body.is_empty() {
        bodies.push(body);
    }
    Ok(bodies)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bodies(payloads: &[&str], max_bytes: usize) -> Vec<String> {
        chunk(payloads.iter().map(|p| p.as_bytes()), max_bytes)
            .unwrap()
            .into_iter()
            .map(|body| String::from_utf8(body).unwrap())
            .collect()
    }

    #[test]
    fn payloads_under_the_limit_share_one_body() {
        assert_eq!(
            bodies(&[r#"{"id":1}"#, r#" {"id":2} "#], usize::MAX),
            vec!["{\"id\":1}\n{\"id\":2}\n"]
        );
        assert!(bodies(&[], 10).is_empty());
    }

    #[test]
    fn a_body_is_split_in_order_before_it_exceeds_the_limit() {
        let line = r#"{"id":1}"#;
        assert_eq!(
            bodies(&[line, line, line], 18),
            vec!["{\"id\":1}\n{\"id\":1}\n", "{\"id\":1}\n"]
        );
    }

    #[test]
    fn a_payload_over_the_limit_is_sent_alone() {
        assert_eq!(
            bodies(
                &[
                    r#"{"id":1}"#,
                    r#"{"long":"xxxxxxxxxxxxxxxx"}"#,
                    r#"{"id":3}"#
                ],
                12
            ),
            vec![
                "{\"id\":1}\n",
                "{\"long\":\"xxxxxxxxxxxxxxxx\"}\n",
                "{\"id\":3}\n"
            ]
        );
    }

    #[test]
    fn a_multi_line_payload_is_put_on_one_line() {
        assert_eq!(bodies(&["{\n \"id\": 1\n}"], 100), vec!["{\"id\":1}\n"]);
    }

    #[test]
    fn a_payload_that_is_not_an_object_is_refused() {
        assert!(chunk([b"[1,2]".as_slice()], 100).is_err());
        assert!(chunk([b"{\n broken".as_slice()], 100).is_err());
        assert!(chunk([b"{ broken".as_slice()], 100).is_err());
        assert!(chunk([br#"{"id":1} x"#.as_slice()], 100).is_err());
    }
}
