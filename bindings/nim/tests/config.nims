import std/os

switch("path", thisDir() / ".." / "src")
switch("mm", "orc")
switch("threads", "on")

let lib = getEnv("MQB_LIB")
if lib.len > 0:
  switch("define", "mqbLib=" & lib)
else:
  let name = when defined(macosx): "libmq_bridge.dylib"
             elif defined(windows): "mq_bridge.dll"
             else: "libmq_bridge.so"
  for profile in ["debug", "release"]:
    let candidate = thisDir() / ".." / ".." / ".." / "target" / profile / name
    if fileExists(candidate):
      switch("define", "mqbLib=" & candidate)
      break

when defined(addressSanitizer):
  switch("debugger", "native")
  switch("define", "noSignalHandler")
  switch("define", "useMalloc")
  switch("passC", "-fsanitize=address -fno-omit-frame-pointer")
  switch("passL", "-fsanitize=address -fno-omit-frame-pointer")
