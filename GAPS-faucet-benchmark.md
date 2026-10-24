# Gaps aus dem faucet-Benchmark

1. Lokaler `object_store` fsynct jede Part-Datei (unten, erster Teil).
2. `concurrency: 1` überlappt Lesen und Schreiben nicht (zweiter Teil, am Ende).

# Gap 1: lokaler `object_store` fsynct jede Part-Datei

Stand 2026-10-23, mq-bridge 1.1.0 (`mqb` aus Homebrew). Gefunden beim Benchmark in
`../mq-bridge-faucet/bench/run.py` (Fall `4 parquet write`, Variante `native`).

## Befund

Route: `file` (JSONL, `format: raw`, 1 Mio. Zeilen) → `object_store`
`{"url": "local-store://…/f/", "format": "parquet"}`, Standard-Batch, `concurrency: 1`.

| Version | Zeit | Dateien |
|---|---|---|
| 1.0.3 | 5,5 s | 1.954 |
| 1.1.0 | 21,8 s | 1.954 |
| 1.1.0, `batch_size: 5000` | 4,6 s | 201 |
| 1.1.0, `batch_size: 50000` | 2,75 s | 21 |

Profil (`sample`): 5,4 s CPU bei 24 s Laufzeit. Die Blocking-Threads hängen in
`open` → `fcntl` (`F_FULLFSYNC`) → `linkat` → `fcntl`, also in
`LocalFileSystem::put_opts` mit `PutMode::Create` und `fsync = true`.

Ursache ist Commit `ea465638` ("fsync"), `src/checkpoint.rs` `build_store`:

```rust
// A remote PUT is durable once it returns; the local store only matches that with fsync.
let local = object_store::local::LocalFileSystem::new().with_fsync(true);
```

`build_store` wird auch vom `object_store`-Endpoint benutzt. Pro Part-Datei laufen damit
mindestens zwei volle Syncs (Datei, dann das Verzeichnis nach dem Hard-Link). Auf macOS
ist `sync_all` ein `F_FULLFSYNC`, das etwa 5 ms kostet.

## Warum das nicht einfach ein Fehler ist

Bei `name_by: source_position` (dem aufgelösten `auto`, sobald der Input Positionen
liefert) liest `recover_finalized_object_ranges` nach einem Neustart nur die
Dateinamen. Eine benannte Datei, deren Inhalt ein Stromausfall verloren hat, würde als
geschrieben gelten, und der Bereich würde beim Replay übersprungen: Datenverlust. Für
Source-Position-Parts ist der Sync also nötig. Die `file`-Regel macht dieselbe Ausnahme
(`sync_file_sinks_behind_saved_position`: "part files always sync").

Bei `name_by: write_time` schützt der Sync dagegen nur, wenn der Input seine Position
speichert. Liest er beim Neustart alles neu, geht nichts verloren, was nicht neu
geschrieben wird.

## Empfehlung

1. **Regel wie beim `file`-Sink.** Lokaler `object_store`-Sink:
   - `source_position`: immer syncen (wie heute).
   - `write_time`: nur syncen, wenn der Input seine Position speichert
     (`input_saves_position`), sonst nicht.
   - Den Checkpoint-Store selbst weiterhin immer mit fsync bauen. Dafür `build_store`
     teilen: eine Variante für den Checkpoint (immer fsync), eine für den Sink (fsync
     nach Regel).
2. **Option zum Übersteuern:** `fsync: off | batch` am `object_store`, unset = Regel aus 1.
   Wirkt nur bei `file://` bzw. `local-store://`. `off` bei `source_position` mit
   `warn!` und Hinweis auf das Replay-Risiko, analog zum `info!` beim `file`-Sink mit
   `fsync: off`. URI-Form, `mqb describe`, `mq-bridge.schema.json` und
   docs/DELIVERY.md mitziehen (beim `file`-Sink fehlte `fsync` zuerst in der Config-Form).
3. **Doku:** In docs/DELIVERY.md erwähnen, dass bei lokalem `object_store` die Zahl der
   Syncs der Zahl der Part-Dateien entspricht. Für Durchsatz hilft ein größeres
   `batch_size` bei voller Sicherheit, siehe Tabelle oben.

Nicht empfohlen: den Sync für `source_position` ohne Ersatz abschalten, oder Parts erst
gesammelt im Deferred-Commit-Intervall syncen. Das Zweite geht nur, wenn Parts bis zum
Sync unter einem Staging-Namen liegen (`.stage-`), damit die Recovery sie nicht als
geschrieben zählt. Das ist deutlich mehr Umbau, und `LocalFileSystem` lässt sich dafür
nicht von außen anpassen.

## Prüfen

- Test: lokaler Sink mit `write_time` und einem Input ohne gespeicherte Position syncen
  nicht; mit gespeicherter Position syncen sie; `source_position` syncen immer;
  `fsync: off` übersteuert.
- Benchmark: `cd ../mq-bridge-faucet && ONLY=4 VARIANT=native uv run --with pyarrow
  python bench/run.py` gegen ein neu gebautes `mqb`. Mit `name_by: write_time` sollte es
  wieder etwa bei 5 s liegen. Der Default (`source_position`) bleibt bei etwa 22 s.

# Gap 2: `concurrency: 1` überlappt Lesen und Schreiben nicht

## Befund

`Route::run_sequentially` (src/route.rs) liest einen Batch, sendet ihn, wartet auf das
Ergebnis und liest erst dann den nächsten. Der Default ist uneinheitlich:

- Bibliothek und alle Routen aus einer Config-Datei (`mqb -c`): `concurrency: 1`
  (`src/models/defaults.rs` `default_concurrency()`, unverändert seit dem ersten
  Commit, also auch in 0.4.x und 1.0.x).
- `mqb copy` und MCP (`start_route`, `generate_cli_command`): 4
  (`apps/mq-bridge-app/crates/cli/src/main.rs` `DEFAULT_CONCURRENCY`, seit dem Merge
  der App ins Repo, 83f5b272, 2026-08-28).

Diese Lücke betrifft also Config-Routen. faucet selbst hat ebenfalls nur einen Schreiber, verbindet
Quelle und Sink aber über einen Kanal mit 4 Seiten Puffer
(`faucet-core` `topology.rs`, `DEFAULT_CHANNEL_CAPACITY = 4`). Dort liest die Quelle
also weiter, während der Sink schreibt.

Messung, 1 Mio. Zeilen, `file` (JSONL) → Sink, je ein Lauf:

| Sink | `concurrency: 1` | `2` | `4` | faucet CLI |
|---|---|---|---|---|
| nativ `file` JSONL | 1,48 s | 0,82 s | 0,82 s | – |
| Plugin `jsonl`, `flush: periodic` | 2,35 s | 1,35 s | 1,11 s | 1,21 s |
| Plugin Parquet, `flush: periodic` | 2,87 s | 2,04 s | 1,92 s | 1,95 s |
| Plugin `sqlite` | 2,67 s | 1,61 s | 1,56 s | 1,83 s |

Ab `concurrency: 2` ist mq-bridge gleich schnell wie faucet oder schneller. Mit dem
Bibliotheks-Default (Config-Routen) ist es bis zu doppelt so langsam.

## Warum nicht einfach den Bibliotheks-Default auf 4 setzen wie in der App

- Oberhalb von 1 rufen Worker `send_batch` parallel auf. Ohne
  `requires_ordered_publish() == true` erreichen ganze Batches den Sink dann außer
  Reihenfolge (docs/ARCHITECTURE.md). Das betrifft jeden Sink, der das nicht
  deklariert: Kafka, MQTT, NATS, HTTP, Plugins. Gemessen beim faucet-Plugin vor dem
  Fix: 9 vertauschte Batches bei `concurrency: 4`, nativ `file` 0. Ein höherer Default
  würde die Reihenfolge für bestehende Routen stillschweigend ändern, also ein
  Breaking Change.
- Mehr Batches gleichzeitig im Speicher, mehr parallele Verbindungen zum Ziel.

## Empfehlung

1. **Ein Batch Vorlauf in `run_sequentially`:** den nächsten `receive_batch` starten,
   während `send_batch_and_commit` des vorigen läuft. Gesendet wird weiter einer nach
   dem anderen in Quellreihenfolge. Damit ändert sich keine Garantie, nur die
   Wartezeit. Commits laufen weiter über den `commit_router` in Reihenfolge. Zu
   prüfen: Shutdown und Reconnect mit einem vorausgelesenen, noch nicht gesendeten
   Batch (er darf nicht committet werden, sondern wird nach dem Reconnect neu
   gelesen), `exit_on_empty` und `pause_after_empty_batch`.
2. **Alternativ nur Doku:** In docs/CONFIGURATION.md bzw. DELIVERY.md erklären, dass
   `concurrency: 1` Lesen und Schreiben nicht überlappt und `concurrency: 2` bei
   Sinks mit `requires_ordered_publish` (z. B. `file`) die Reihenfolge behält und
   etwa doppelt so schnell ist.

3. **Defaults angleichen oder dokumentieren:** Dass `mqb copy` 4 nimmt und dieselbe
   Route aus `-c` nur 1, ist überraschend. Mit Punkt 1 kann der Bibliotheks-Default
   bei 1 bleiben, weil 1 dann kaum noch langsamer ist. Ohne Punkt 1 wenigstens den
   Unterschied in der Doku nennen. 4 auch für Config-Routen ginge nur als Breaking
   Change (Reihenfolge, siehe oben).

## Prüfen

`cd ../mq-bridge-faucet && ROUTE='{"concurrency": 1}' ONLY=3,5 uv run --with pyarrow
python bench/run.py` gegen ein neu gebautes `mqb`. Mit Vorlauf sollte
`concurrency: 1` nahe an die Werte für `2` kommen.
