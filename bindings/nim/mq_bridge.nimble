version       = "1.5.0"
author        = "Marco Mengelkoch"
description   = "Nim binding of mq-bridge: move messages between brokers, databases, files and HTTP"
license       = "MIT OR Apache-2.0"
srcDir        = "src"

requires "nim >= 2.0.0"

import std/os

proc getLib() =
  ## Downloads this release's C library into src/lib, where the binding looks first.
  let file = when defined(windows): "mq_bridge.dll"
             elif defined(macosx): "libmq_bridge.dylib"
             else: "libmq_bridge.so"
  let dir = thisDir() / "src" / "lib"
  if fileExists(dir / file) or getEnv("MQB_SKIP_LIB_DOWNLOAD").len > 0:
    return
  let target =
    when defined(windows): "x86_64-pc-windows-msvc"
    elif defined(macosx): (if hostCPU == "arm64": "aarch64-apple-darwin" else: "")
    elif hostCPU == "arm64": "aarch64-unknown-linux-gnu"
    else: "x86_64-unknown-linux-gnu"
  if target.len == 0:
    echo "mq_bridge: no prebuilt library for this platform; build it with cargo (see README)"
    return
  let archive = "mq-bridge-c-" & target & (when defined(windows): ".zip" else: ".tar.gz")
  let url = "https://github.com/marcomq/mq-bridge/releases/download/v" & version & "/" & archive
  let tmp = dir / "download"
  mkDir tmp
  let (output, code) = gorgeEx("curl -fsSL -o \"" & tmp / archive & "\" " & url &
                               " && tar -xf \"" & tmp / archive & "\" -C \"" & tmp & "\"")
  if code == 0:
    cpFile(tmp / "lib" / file, dir / file)
    echo "mq_bridge: downloaded ", file, " ", version, " to ", dir
  else:
    echo "mq_bridge: could not download ", url, ": ", output
    echo "mq_bridge: get the library yourself (see README) and compile with -d:mqbLib=<path>"
  rmDir tmp

task getlib, "Download the C library of this release into src/lib":
  getLib()

before install:
  getLib()

task test, "Run the tests against libmq_bridge (set MQB_LIB or build bindings/c first)":
  for test in ["tdrift", "tmessage", "tclient", "troute", "tplugin"]:
    exec "nim c -r --hints:off tests/" & test & ".nim"
