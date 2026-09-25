//! Plugins written in C against `include/mq_bridge_plugin.h` load and run.
//!
//! Builds `examples/c-plugin` with the system C compiler (`$CC`, default `cc`),
//! so the headers, the examples and the host are checked together.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use mq_bridge::endpoints::memory::get_or_create_channel;
use mq_bridge::extensions::get_endpoint_factory;
use mq_bridge::models::{MemoryConfig, Route};
use mq_bridge::plugin::load_endpoint_plugin;
use mq_bridge::support::plugin_abi::{
    MqbPluginEntry, MQB_PLUGIN_ENTRY_SYMBOL, MQB_PLUGIN_LIST_SYMBOL,
};
use mq_bridge::CanonicalMessage;
use serde_json::{json, Value};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn run(mut command: Command) {
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("could not run {command:?}: {err}"));
    assert!(
        output.status.success(),
        "{command:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Compiles `sources` from `examples/c-plugin` into `lib<name>` and checks its table.
fn build_c_plugin(name: &str, sources: &[&str]) -> PathBuf {
    let root = repo_root();
    let example = root.join("examples/c-plugin");
    let dir = std::env::temp_dir().join(format!("mqb-c-plugin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the build directory");
    let library = dir.join(format!("lib{name}.{}", std::env::consts::DLL_EXTENSION));

    let mut cc = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()));
    cc.args([
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Wno-unused-parameter",
        "-Werror",
        "-fPIC",
    ])
    .arg(if cfg!(target_os = "macos") {
        "-dynamiclib"
    } else {
        "-shared"
    })
    .arg("-I")
    .arg(root.join("include"))
    .args(sources.iter().map(|source| example.join(source)))
    .arg("-o")
    .arg(&library);
    run(cc);
    assert_every_entry_is_set(&library);
    library
}

/// The helper macros are hand-written, so an entry appended to the ABI but not
/// to a macro would be left null. The host never checks, so check here.
fn assert_every_entry_is_set(library: &Path) {
    // struct_size, the abi_major/abi_minor pair, capabilities, name and version.
    const HEADER_WORDS: usize = 7;
    unsafe {
        let library = libloading::Library::new(library).expect("open the library");
        let entry = library
            .get::<MqbPluginEntry>(MQB_PLUGIN_ENTRY_SYMBOL)
            .expect("the entry symbol");
        let table = entry();
        let words = (*table).struct_size / size_of::<usize>();
        let words = std::slice::from_raw_parts(table.cast::<usize>(), words);
        for (index, word) in words.iter().enumerate().skip(HEADER_WORDS) {
            assert_ne!(*word, 0, "function entry {} is null", index - HEADER_WORDS);
        }
    }
}

/// Builds and loads `legacy_payments` once; several tests share it.
fn load_legacy_payments() {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| {
        let library = build_c_plugin(
            "legacy_payments",
            &["plugin.c", "legacy_parser.c", "legacy_ledger.c"],
        );
        let info = load_endpoint_plugin(library).expect("load the C plugin");
        assert_eq!(info.name, "legacy_payments");
        assert!(info.supports_middleware && info.supports_publisher);
        assert!(!info.supports_consumer);
    });
}

/// Runs a memory route with `middleware` on its input and returns what arrived.
async fn run_through(middleware: &str, input: Vec<CanonicalMessage>) -> Vec<CanonicalMessage> {
    let (source, sink) = (format!("{middleware}-in"), format!("{middleware}-out"));
    let input_channel = get_or_create_channel(&MemoryConfig::new(source.clone(), None));
    let output_channel = get_or_create_channel(&MemoryConfig::new(sink.clone(), None));
    input_channel
        .fill_messages(input)
        .await
        .expect("fill the input");
    input_channel.close();

    let route: Route = serde_json::from_value(json!({
        "input": {
            "memory": { "topic": source },
            "middlewares": [{ "custom": { "name": middleware, "config": {} } }]
        },
        "output": { "memory": { "topic": sink } }
    }))
    .expect("route config");
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        route.run_until_err(middleware, None, None),
    )
    .await;
    output_channel.drain_messages()
}

#[test]
fn the_headers_compile_as_cpp() {
    let mut cxx = Command::new(std::env::var("CXX").unwrap_or_else(|_| "c++".into()));
    cxx.args([
        "-std=c++11",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-fsyntax-only",
        "-x",
        "c++",
    ])
    .arg(repo_root().join("include/mq_bridge_plugin.h"));
    run(cxx);
}

#[test]
fn the_header_names_the_symbols_the_host_loads() {
    let header = std::fs::read_to_string(repo_root().join("include/mq_bridge_plugin.h"))
        .expect("read the header");
    for (name, value) in [
        ("MQB_PLUGIN_ENTRY_SYMBOL", MQB_PLUGIN_ENTRY_SYMBOL),
        ("MQB_PLUGIN_LIST_SYMBOL", MQB_PLUGIN_LIST_SYMBOL),
    ] {
        let symbol = std::str::from_utf8(value.strip_suffix(b"\0").unwrap()).unwrap();
        let define = format!("#define {name} \"{symbol}\"");
        assert!(header.contains(&define), "header lacks `{define}`");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_minimal_c_middleware_passes_kept_messages_through_unchanged() {
    let info = load_endpoint_plugin(build_c_plugin("drop_heartbeats", &["minimal.c"]))
        .expect("load the C plugin");
    assert_eq!(info.name, "drop_heartbeats");
    assert!(info.supports_middleware);

    let order = CanonicalMessage::from("order-1").with_metadata_kv("source", "atm");
    let order_id = order.message_id;
    let messages = run_through(
        "drop_heartbeats",
        vec![CanonicalMessage::from("ping"), order],
    )
    .await;

    assert_eq!(messages.len(), 1, "the heartbeat is dropped");
    assert_eq!(messages[0].get_payload_str(), "order-1");
    assert_eq!(messages[0].message_id, order_id);
    assert_eq!(
        messages[0].metadata.get("source").map(String::as_str),
        Some("atm")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_c_middleware_parses_and_drops_records_in_a_route() {
    load_legacy_payments();
    let messages = run_through(
        "legacy_payments",
        vec![
            CanonicalMessage::from("DE12345678000000012345EUR").with_metadata_kv("source", "atm"),
            CanonicalMessage::from("not a record"),
        ],
    )
    .await;

    assert_eq!(messages.len(), 1, "the invalid record is dropped");
    let payload: Value = serde_json::from_slice(&messages[0].payload).expect("JSON payload");
    assert_eq!(
        payload,
        json!({ "account": "DE12345678", "amount_minor": 12345, "currency": "EUR" })
    );
    assert_eq!(
        messages[0].metadata.get("source").map(String::as_str),
        Some("atm")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocking_c_publisher_writes_through_the_host_fallback() {
    load_legacy_payments();
    let factory = get_endpoint_factory("legacy_payments").expect("registered");
    let path = std::env::temp_dir().join(format!("mqb-ledger-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let publisher = factory
        .create_publisher("ledger", &json!({ "path": path }))
        .await
        .expect("open the ledger");
    for batch in [vec!["a", "b"], vec!["c"]] {
        let batch = batch.into_iter().map(CanonicalMessage::from).collect();
        publisher.send_batch(batch).await.expect("send");
    }
    publisher.flush().await.expect("flush");
    let written = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, "a\nb\nc\n");
    assert!(
        publisher.status().await.healthy,
        "unsupported status is the default"
    );

    let error = factory
        .create_publisher("ledger", &json!({}))
        .await
        .err()
        .expect("a config without a path is rejected");
    assert!(
        format!("{error:#}").contains("config needs a \"path\""),
        "{error:#}"
    );
}

/// A consumer with a non-blocking receive but the stub `batch_commit_async`.
const ASYNC_SOURCE: &str = r#"
#include "mq_bridge_plugin_helpers.h"

static uint8_t batch_token;
static const MqbMessage message = {{1}, {(const uint8_t *)"hi", 2}, NULL, 0};
static size_t blocking_commits;
static uint8_t last_disposition = 0xff;

size_t async_source_blocking_commits(void) { return blocking_commits; }
uint8_t async_source_last_disposition(void) { return last_disposition; }

static MqbStatus create(MqbFactoryHandle factory, MqbSlice route_name, MqbSlice config_json,
                        MqbConsumerHandle *out, MqbBuffer *err) {
    *out = &batch_token;
    return MQB_OK;
}
static MqbStatus receive_async(MqbConsumerHandle consumer, size_t max_messages,
                               MqbBatchHandle *out_batch, const MqbMessage **out_messages,
                               size_t *out_len, MqbBuffer *err, MqbCompletion completion) {
    *out_batch = &batch_token;
    *out_messages = &message;
    *out_len = 1;
    completion.callback(completion.ctx, MQB_OK);
    return MQB_OK;
}
static MqbStatus commit(MqbBatchHandle batch, const uint8_t *dispositions, size_t len,
                        MqbBuffer *err) {
    blocking_commits++;
    last_disposition = dispositions[0];
    return MQB_OK;
}

static const MqbPluginVTable table = {
    MQB_TABLE_HEADER("async_source", "0.1.0", MQB_CAP_CONSUMER),
    MQB_DEFAULT_FACTORY,
    MQB_NO_PUBLISHER,
    MQB_NO_MIDDLEWARE,
    .consumer_create = create, .consumer_receive_batch = mqb_stub_receive,
    .consumer_commit_requires_order = mqb_stub_false,
    .consumer_set_exit_on_empty = mqb_stub_set_exit_on_empty,
    .consumer_close = mqb_stub_handle, .consumer_free = mqb_stub_free,
    .batch_commit = commit, .batch_free = mqb_stub_free,
    .batch_commit_replies = mqb_stub_commit_replies, .consumer_status = mqb_stub_status,
    .consumer_receive_batch_async = receive_async,
    .batch_commit_async = mqb_stub_commit_async,
};

const MqbPluginVTable *mq_bridge_plugin_v1(void) { return &table; }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_async_commit_falls_back_to_the_blocking_one() {
    use mq_bridge::traits::MessageDisposition;

    let source = std::env::temp_dir().join(format!("mqb-async-source-{}.c", std::process::id()));
    std::fs::write(&source, ASYNC_SOURCE).expect("write the C source");
    let library = build_c_plugin("async_source", &[source.to_str().unwrap()]);
    load_endpoint_plugin(&library).expect("load the C plugin");
    let factory = get_endpoint_factory("async_source").expect("registered");
    assert!(
        factory.acknowledges(&json!({})),
        "MQB_DEFAULT_FACTORY reports the schema default"
    );

    let mut consumer = factory
        .create_consumer("async_source", &json!({}))
        .await
        .expect("open the consumer");
    for disposition in [MessageDisposition::Ack, MessageDisposition::Nack] {
        let batch = consumer.receive_batch(1).await.expect("receive");
        assert_eq!(batch.messages[0].get_payload_str(), "hi");
        (batch.commit)(vec![disposition]).await.expect("commit");
    }

    let (commits, last) = unsafe {
        let library = libloading::Library::new(&library).expect("open the library");
        let commits = library
            .get::<unsafe extern "C" fn() -> usize>(b"async_source_blocking_commits\0")
            .unwrap();
        let last = library
            .get::<unsafe extern "C" fn() -> u8>(b"async_source_last_disposition\0")
            .unwrap();
        (commits(), last())
    };
    assert_eq!(
        commits, 2,
        "each batch committed through the blocking entry"
    );
    assert_eq!(last, mq_bridge::support::plugin_abi::MQB_DISPOSITION_NACK);
}
