window.BENCHMARK_DATA = {
  "lastUpdate": 1767555808141,
  "repoUrl": "https://github.com/marcomq/mq-bridge",
  "entries": {
    "Rust Benchmark": [
      {
        "commit": {
          "author": {
            "name": "marcomq",
            "username": "marcomq"
          },
          "committer": {
            "name": "marcomq",
            "username": "marcomq"
          },
          "id": "7a1a2644910285e81f97b8b7120184ed87114cec",
          "message": "Add AWS SQS / SNS support",
          "timestamp": "2026-01-02T23:43:35Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/8/commits/7a1a2644910285e81f97b8b7120184ed87114cec"
        },
        "date": 1767555807820,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1421811761,
            "range": "± 43987052",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2943755225,
            "range": "± 30705725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 212900823,
            "range": "± 10860603",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 327601650,
            "range": "± 10616001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1909242881,
            "range": "± 230380771",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5439444,
            "range": "± 853038",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 1044644351,
            "range": "± 25628894",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3948498,
            "range": "± 1323539",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25890703,
            "range": "± 2251753",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 21403469,
            "range": "± 3407542",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23515719,
            "range": "± 1016081",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 20072689,
            "range": "± 1876828",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16646258,
            "range": "± 2654120",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9936085,
            "range": "± 242820",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 50253357,
            "range": "± 5014714",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9474088,
            "range": "± 302736",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 83549166,
            "range": "± 9690962",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 531832081,
            "range": "± 24076211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 10694001,
            "range": "± 638325",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 54741673,
            "range": "± 4185387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 2778242,
            "range": "± 49595",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 12650669,
            "range": "± 150483",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 2805436,
            "range": "± 55448",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 12521788,
            "range": "± 144115",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}