#!/usr/bin/env python3
"""Bridge CPU per request without a database: where route.yaml's overhead goes.

- static_inline: http input -> static output, the inline fast path (hyper + tokio floor)
- lookup_static: the normal route pipeline with a `lookup` from a `static` endpoint
- switch3_static: a `when` switch whose third predicate matches, as `/feed` does in route.yaml
- feed_lookup_output / feed_lookup_input: route.yaml's feed query (sqlx, pool 1) as a lookup on
  the output or on the input, answered by a static output
"""
import socket
import subprocess
import sys
import shutil
import time

from run import APP, SEED, WORK, cpu_seconds, oha

PORT = 13001
STATIC = '{ static: { body: "{\\"ok\\":1}", raw: true, metadata: { content-type: application/json } } }'
LOOKUP_STATIC = ('{ static: { body: "${payload:j | raw}", raw: true }, middlewares: [ { lookup: '
                 '{ from: { static: { body: "{\\"j\\":\\"x\\"}", raw: true } }, into: $ } } ] }')


def when(cond):
    return f"{{ if: '{cond}', to: {STATIC} }}"


SWITCH = ("{ switch: { when: [ "
          + ", ".join([when('meta.http_method == "POST" and meta.http_path == "/posts"'),
                       when('meta.http_method == "GET" and startsWith(meta.http_path, "/posts/")'),
                       when('meta.http_method == "GET" and meta.http_path == "/feed"')])
          + " ] } }")
DB = WORK / "decompose.db"
FEED = ("SELECT json_object('posts', json_group_array(json(x))) AS j FROM (SELECT json_object('id',p.id,"
        "'body',p.body,'created_at',p.created_at,'author',u.username,'like_count',(SELECT count(*) FROM likes l "
        "WHERE l.post_id=p.id)) AS x FROM posts p JOIN users u ON u.id=p.user_id "
        "ORDER BY p.created_at DESC, p.id DESC LIMIT 20)")
FEED_LOOKUP = (f'{{ lookup: {{ from: {{ sqlx: {{ url: "sqlite://{DB}", table: posts, max_connections: 1, '
               f'lookup_query: "{FEED}" }} }}, into: $ }} }}')
REPLY = '{ static: { body: "${payload:j | raw}", raw: true, metadata: { content-type: application/json } } }'
CASES = {
    "static_inline": ("", STATIC),
    "lookup_static": ("", LOOKUP_STATIC),
    "switch3_static": ("", SWITCH),
    "feed_lookup_output": ("", REPLY[:-2] + f", middlewares: [ {FEED_LOOKUP} ] }}"),
    "feed_lookup_input": (f", middlewares: [ {FEED_LOOKUP} ]", REPLY),
}


def main():
    WORK.mkdir(exist_ok=True)
    shutil.copy(SEED, DB)
    args = ["--rand-regex-url", f"http://127.0.0.1:{PORT}/feed"]
    for name, (input_mw, output) in CASES.items():
        config = (f"log_level: warn\nroutes:\n  r:\n    concurrency: 1\n    batch_size: 512\n"
                  f"    input: {{ http: {{ url: \"127.0.0.1:{PORT}\" }}{input_mw} }}\n    output: {output}\n")
        proc = subprocess.Popen([APP, "--config-str", config, "--color", "never"],
                                stdout=open(WORK / f"{name}.log", "w"), stderr=subprocess.STDOUT)
        try:
            while True:
                try:
                    socket.create_connection(("127.0.0.1", PORT), 0.2).close()
                    break
                except OSError:
                    if proc.poll() is not None:
                        raise RuntimeError(f"{APP} exited, see {WORK / name}.log")
                    time.sleep(0.1)
            oha(args, "2s", 64)
            before = cpu_seconds(proc.pid)
            result = oha(args, "6s", 64)
            spent = cpu_seconds(proc.pid) - before
            done = sum(result["statusCodeDistribution"].values())
            print(f"{name:15} {spent / done * 1e6:6.1f} µs/req  {result['summary']['requestsPerSec']:8.0f} req/s",
                  flush=True)
        finally:
            proc.terminate()
            proc.wait(10)


if __name__ == "__main__":
    sys.exit(main())
