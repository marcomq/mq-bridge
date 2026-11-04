import std/os

switch("path", thisDir() / ".." / "src")
switch("outdir", thisDir() / "out")
switch("hints", "off")

let lib = getEnv("MQB_LIB")
if lib.len > 0:
  switch("define", "mqbLib=" & lib)
else:
  let name = when defined(macosx): "libmq_bridge.dylib"
             elif defined(windows): "mq_bridge.dll"
             else: "libmq_bridge.so"
  for profile in ["release", "debug"]:
    let candidate = thisDir() / ".." / ".." / ".." / "target" / profile / name
    if fileExists(candidate):
      switch("define", "mqbLib=" & candidate)
      break
