# Fails when raw.nim and the C headers in bindings/c/include disagree: a
# function on one side only, a constant with another value, a struct of another size.

import std/[unittest, macros, os, sets, strutils]
import mq_bridge/raw

const
  includeDir = currentSourcePath.parentDir / ".." / ".." / "c" / "include"
  headers = ["mq_bridge.h", "mq_bridge_plugin.h"]
  rawSource = staticRead(currentSourcePath.parentDir / ".." / "src" / "mq_bridge" / "raw.nim")

proc headerText(): string {.compileTime.} =
  for h in headers: result.add staticRead(includeDir / h) & "\n"

proc functionNames(text: string, before: set[char], after: char,
                   procOnly = false): HashSet[string] =
  ## Every `mqb_*` name preceded by a char in `before` and followed by `after`.
  var i = text.find("mqb_")
  while i >= 0:
    var j = i + "mqb_".len
    while j < text.len and text[j] in IdentChars: inc j
    let lead = i > 0 and text[i - 1] in before and (procOnly == false or
      (i >= 5 and text[i - 5 ..< i] == "proc "))
    if lead and j < text.len and text[j] == after:
      result.incl text[i ..< j]
    i = text.find("mqb_", j)

macro checkConstants(): untyped =
  ## For every integer `#define MQB_*` that raw.nim declares, compares the Nim
  ## value with the one the C compiler sees.
  result = newStmtList()
  let header = includeDir / "mq_bridge.h"
  for line in headerText().splitLines:
    let parts = line.splitWhitespace()
    if parts.len < 3 or parts[0] != "#define" or not parts[1].startsWith("MQB_") or
        '(' in parts[1] or '"' in line:
      continue
    let name = parts[1]
    let nimName = ident(name)
    let cName = genSym(nskVar, "c")
    result.add quote do:
      when declared(raw.`nimName`):
        var `cName` {.importc: `name`, header: `header`, nodecl.}: int64
        check int64(raw.`nimName`) == `cName`

template cSize(cType: static string): int =
  var size {.importc: "sizeof(" & cType & ")", header: includeDir / "mq_bridge.h", nodecl.}: csize_t
  int(size)

suite "raw.nim matches the C headers":
  test "the same functions":
    let fromHeader = functionNames(static(headerText()), {' ', '*'}, '(')
    let fromNim = functionNames(rawSource, {' '}, '*', procOnly = true)
    check fromHeader - fromNim == initHashSet[string]()
    check fromNim - fromHeader == initHashSet[string]()

  test "the same constant values":
    checkConstants()

  test "the same struct sizes":
    check sizeof(MqbSlice) == cSize("struct MqbSlice")
    check sizeof(MqbBuffer) == cSize("struct MqbBuffer")
    check sizeof(MqbKeyValue) == cSize("struct MqbKeyValue")
    check sizeof(MqbMessage) == cSize("struct MqbMessage")
    check sizeof(MqbCompletion) == cSize("struct MqbCompletion")
    check sizeof(MqbCrashInfo) == cSize("struct MqbCrashInfo")
    check sizeof(MqbHostVTable) == cSize("struct MqbHostVTable")
    check sizeof(MqbPluginVTable) == cSize("struct MqbPluginVTable")
