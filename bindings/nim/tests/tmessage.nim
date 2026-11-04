import std/[unittest, json, strutils]
import mq_bridge

suite "message":
  test "payload, id and metadata round-trip":
    var m = newMessage("hello", id = "42", metadata = {"kind": "greeting"})
    m.setMetadata("lang", "en")
    check m.payload == "hello"
    check m.id.len > 0
    check m.metadata("kind") == "greeting"
    check m.metadata("missing", "fallback") == "fallback"
    check m.hasMetadata("lang")
    check m.metadata.len == 2

  test "copies are independent":
    var a = newMessage("a", metadata = {"k": "1"})
    var b = a
    b.setMetadata("k", "2")
    check a.metadata("k") == "1"
    check b.metadata("k") == "2"
    check b.payload == "a"

  test "json payload":
    let m = newMessage(%*{"id": 1})
    check m.json["id"].getInt == 1

  test "empty payload":
    check newMessage("").payload == ""

  test "version matches":
    check version().count('.') == 2
