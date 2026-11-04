## Read a CSV file, type and enrich every row in Nim, drop the bad ones, and write
## the rest twice: as JSON lines and as a Parquet dataset other tools can query.

import std/[json, math, options, os, strutils]
import mq_bridge

const here = currentSourcePath().parentDir
var dropped: seq[string]

proc dewPoint(t, rh: float): float =
  ## Magnus formula, accurate to about 0.4 °C between -45 and 60 °C.
  let g = ln(rh / 100) + 17.62 * t / (243.12 + t)
  round(243.12 * g / (17.62 - g), 1)

proc enrich(m: MessageView): Option[Message] =
  let row = m.json                                   # CSV fields are all strings
  try:
    let t = round(row["temp_c"].getStr.parseFloat, 2)
    let rh = round(row["humidity"].getStr.parseFloat, 2)
    if t notin -50.0 .. 60.0 or rh notin 0.0 .. 100.0:
      raise newException(ValueError, "out of range")
    some(newMessage(%*{
      "station": row["station"], "time": row["time"], "day": row["time"].getStr[0 ..< 10],
      "temp_c": t, "humidity": rh, "dew_point_c": dewPoint(t, rh),
      "frost_risk": dewPoint(t, rh) <= 0}))
  except ValueError:
    dropped.add row["station"].getStr & " " & row["time"].getStr
    none(Message)                                    # acknowledge, write nothing

let outDir = here / "out"
removeDir(outDir / "readings")
removeFile(outDir / "readings.jsonl")
createDir(outDir / "readings")

Route.fromStr("""
exit_on_empty: true
batch_size: 10000
input:
  file: { path: "$1", format: csv }
output:
  fanout:
    - file: { path: "$2/readings.jsonl", format: raw }
    - object_store: { url: "file://$2/readings", format: parquet, compression: zstd,
                    name_by: write_time, date_partition_style: hive }
""".multiReplace(("$1", here / "data" / "readings.csv"), ("$2", outDir)))
  .withHandler(enrich)
  .run()

echo readFile(outDir / "readings.jsonl")
echo "dropped: ", dropped
for file in walkDirRec(outDir / "readings"): echo "parquet: ", file
