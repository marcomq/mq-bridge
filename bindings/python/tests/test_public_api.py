from mq_bridge import (
    MemoryDrainer,
    Message,
    NonRetryableError,
    Publisher,
    RetryableError,
    Route,
    config_schema,
    init_logging,
    load_plugin_package,
    plugin_library_path,
)


VALID_ID = "018f4aa3-95b5-7cc2-b6d4-a4d82b8cf7a1"


def test_public_exports_are_available() -> None:
    assert MemoryDrainer is not None
    assert Message is not None
    assert NonRetryableError is not None
    assert Route is not None
    assert Publisher is not None
    assert RetryableError is not None
    assert config_schema is not None
    assert init_logging is not None
    assert load_plugin_package is not None
    assert plugin_library_path is not None


def test_config_schema_is_public_api() -> None:
    schema = config_schema()

    assert isinstance(schema, dict)
    assert schema.get("type") == "object"


def test_message_from_json_round_trip_shape() -> None:
    message = Message.from_json(
        {"hello": "world"},
        {"kind": "demo.created"},
        VALID_ID,
    )

    assert message.payload.startswith(b"{")
    assert message.json() == {"hello": "world"}
    assert message.metadata == {"kind": "demo.created"}
    assert message.id == "018f4aa3-95b5-7cc2-b6d4-a4d82b8cf7a1"


def test_message_text_decodes_utf8_payload() -> None:
    message = Message(b"hello", {"kind": "demo.text"})

    assert message.text() == "hello"


def test_message_response_helpers_preserve_metadata_and_id() -> None:
    message = Message(
        b"old",
        {"kind": "demo.reply"},
        VALID_ID,
    )

    json_reply = message.with_json({"ok": True})
    payload_reply = message.with_payload(b"new")

    assert json_reply.json() == {"ok": True}
    assert json_reply.metadata == message.metadata
    assert json_reply.id == message.id
    assert payload_reply.payload == b"new"
    assert payload_reply.metadata == message.metadata
    assert payload_reply.id == message.id


def test_message_accepts_bytes_like_payloads() -> None:
    message = Message(bytearray(b"hello"), {"kind": "demo.bytes"})

    assert message.payload == b"hello"
    assert message.metadata == {"kind": "demo.bytes"}


def test_message_metadata_getter_returns_copy() -> None:
    message = Message(b"hello", {"kind": "demo.copy"})
    metadata = message.metadata

    metadata["kind"] = "mutated"

    assert message.metadata == {"kind": "demo.copy"}


def test_message_repr_includes_shape_without_payload() -> None:
    message = Message(b"secret", {"kind": "demo.repr"}, VALID_ID)

    text = repr(message)

    assert "Message(" in text
    assert "demo.repr" in text
    assert "payload_len=6" in text
    assert "secret" not in text


def test_message_hashes_non_uuid_id_stably() -> None:
    # A non-UUID id is folded into a stable id rather than rejected.
    first = Message(b"hello", id="not-a-uuid")
    second = Message(b"world", id="not-a-uuid")

    assert first.id == second.id
    assert first.id != Message(b"hello", id="another-id").id
    # Pinned FNV-1a/128 vector: the fold must stay identical across processes,
    # releases and the Rust/Node bindings, so a seeded hash would fail here.
    assert first.id == "32f0a7f7-0c3a-5303-92fb-c6935a4bdc48"


def test_message_json_reports_decode_errors() -> None:
    message = Message(b"{")

    try:
        message.json()
    except ValueError as exc:
        assert "failed to decode JSON payload" in str(exc)
    else:  # pragma: no cover - defensive assertion
        raise AssertionError("invalid JSON payload was accepted")


def test_message_text_reports_utf8_errors() -> None:
    message = Message(b"\xff")

    try:
        message.text()
    except ValueError as exc:
        assert "payload is not valid UTF-8" in str(exc)
    else:  # pragma: no cover - defensive assertion
        raise AssertionError("invalid UTF-8 payload was accepted")


def test_message_from_json_rejects_cycles() -> None:
    data = []
    data.append(data)

    try:
        Message.from_json(data)
    except ValueError as exc:
        assert "Cyclic Python container values are not supported" in str(exc)
    else:  # pragma: no cover - defensive assertion
        raise AssertionError("cyclic JSON value was accepted")


def test_route_runtime_api_matches_type_stub() -> None:
    expected_methods = {
        "add_handler",
        "from_yaml",
        "run",
        "stop",
        "with_handler",
    }

    for name in expected_methods:
        assert hasattr(Route, name)


def test_custom_errors_are_catchable_as_exceptions() -> None:
    for error_type in (RetryableError, NonRetryableError):
        try:
            raise error_type("boom")
        except Exception as exc:
            assert str(exc) == "boom"
