# Migrating to 2.0

This page lists what 2.0 changes and how to prepare for it while still on 1.x. Every item
here already works in 1.5, so you can migrate before upgrading.

## Message metadata is no longer a `HashMap`

In 1.x, `CanonicalMessage::metadata` and `MessageContext::metadata` are public
`HashMap<String, String>` fields. Each message costs one allocation for the map and two per
entry. In 2.0 the field becomes an opaque `Metadata` type that can store keys and values
inline or as slices of the received bytes.

1.5 does not warn about the fields by default. To have the compiler list every place that
still uses them, build with the opt-in deprecation:

```sh
RUSTFLAGS="--cfg mqb_deprecate_metadata" cargo check --all-targets
```

Replace each use it reports with the accessor:

| 1.x (`HashMap` field)                    | 1.5+ accessor                                  |
|------------------------------------------|------------------------------------------------|
| `msg.metadata.get("k")`                  | `msg.get_metadata("k")`                        |
| `msg.metadata.insert(k, v)`              | `msg.set_metadata(k, v)`                       |
| `msg.metadata.remove("k")`               | `msg.remove_metadata("k")`                     |
| `msg.metadata.contains_key("k")`         | `msg.has_metadata("k")`                        |
| `msg.metadata.iter()` / `&msg.metadata`  | `msg.metadata_iter()`                          |
| `msg.metadata.keys()`                    | `msg.metadata_iter().map(\|(k, _)\| k)`        |
| `msg.metadata.len()` / `.is_empty()`     | `msg.metadata_len()` / `msg.metadata_is_empty()` |
| `msg.metadata.extend(pairs)`             | `msg.extend_metadata(pairs)`                   |
| `msg.metadata.retain(f)`                 | `msg.retain_metadata(\|k, v\| ...)`            |
| `msg.metadata.clear()`                   | `msg.clear_metadata()`                         |
| `msg.metadata.reserve(n)`                | `msg.reserve_metadata(n)`                      |
| `msg.metadata = map`                     | `msg = msg.with_metadata(map)`                 |
| `msg.metadata["k"]`                      | `msg.get_metadata("k").unwrap()`               |

`MessageContext` has `get_metadata`, `set_metadata`, `remove_metadata` and `metadata_iter`.

Things to watch for:

- `get_metadata` returns `Option<&str>`, not `Option<&String>`. Replace `.cloned()` with
  `.map(str::to_owned)` and drop `.map(|s| s.as_str())`.
- `metadata_iter` yields `(&str, &str)`. Use `.to_owned()` where you used `.clone()`.
- Pass literals and `&str` straight to `set_metadata` and `with_metadata_kv`:
  `set_metadata("kind", "order")`, not `set_metadata("kind".to_string(), ...)`. Calls written
  this way keep compiling in 2.0 and avoid allocations once the store can hold short strings
  inline.
- `with_metadata(HashMap<String, String>)` stays in 2.0, so code that builds a whole map
  keeps working.
- For an owned `HashMap` copy, collect it:
  `msg.metadata_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect()`.

The Python and Node `Message` classes keep exposing metadata as a dict or object.
