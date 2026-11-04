version       = "1.4.0"
author        = "Marco Mengelkoch"
description   = "Nim binding of mq-bridge: move messages between brokers, databases, files and HTTP"
license       = "MIT OR Apache-2.0"
srcDir        = "src"

requires "nim >= 2.0.0"

task test, "Run the tests against libmq_bridge (set MQB_LIB or build bindings/c first)":
  for test in ["tdrift", "tmessage", "tclient", "troute", "tplugin"]:
    exec "nim c -r --hints:off tests/" & test & ".nim"
