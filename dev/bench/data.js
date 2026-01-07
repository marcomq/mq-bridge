window.BENCHMARK_DATA = {
  "lastUpdate": 1767803928636,
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
      },
      {
        "commit": {
          "author": {
            "email": "62469331+marcomq@users.noreply.github.com",
            "name": "Marco Mengelkoch",
            "username": "marcomq"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f359deb12c4a018ec7935bc75ff11979e111726f",
          "message": "Merge pull request #8 from marcomq/dev\n\nAdd AWS SQS / SNS support",
          "timestamp": "2026-01-04T20:52:31+01:00",
          "tree_id": "bbe307a625bd2bce9cbed7915ac443f51050fdbf",
          "url": "https://github.com/marcomq/mq-bridge/commit/f359deb12c4a018ec7935bc75ff11979e111726f"
        },
        "date": 1767557257771,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1410140794,
            "range": "± 48699529",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2948448478,
            "range": "± 46165203",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 210734931,
            "range": "± 16627307",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 326013533,
            "range": "± 15004761",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1459197422,
            "range": "± 192844019",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5077801,
            "range": "± 705566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 1068327283,
            "range": "± 26586191",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3510996,
            "range": "± 941643",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25467090,
            "range": "± 3172577",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20561591,
            "range": "± 3904512",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23738431,
            "range": "± 1652004",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 20128491,
            "range": "± 1909118",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15783329,
            "range": "± 4780627",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9576069,
            "range": "± 85780",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 47380688,
            "range": "± 2388133",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8906125,
            "range": "± 470230",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78698151,
            "range": "± 8248268",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 518706689,
            "range": "± 17491281",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 10497841,
            "range": "± 1241322",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49224807,
            "range": "± 3331437",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 2755289,
            "range": "± 71336",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 12257477,
            "range": "± 126102",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 2758719,
            "range": "± 45612",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 12369482,
            "range": "± 191164",
            "unit": "ns/iter"
          }
        ]
      },
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
          "id": "dc365a5a7c2332172f1453d57a9dacfd4b76dd5d",
          "message": "Add IBM MQ support",
          "timestamp": "2026-01-06T21:06:25Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/9/commits/dc365a5a7c2332172f1453d57a9dacfd4b76dd5d"
        },
        "date": 1767803687939,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1340145952,
            "range": "± 37771483",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2845045603,
            "range": "± 28577746",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 206851470,
            "range": "± 12584789",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 313611007,
            "range": "± 11584493",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1467342986,
            "range": "± 232521505",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5097905,
            "range": "± 475626",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 1044042507,
            "range": "± 48653102",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3495137,
            "range": "± 396686",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25862504,
            "range": "± 2149752",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20662626,
            "range": "± 3099299",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23655279,
            "range": "± 1751886",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19969203,
            "range": "± 2055105",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15775430,
            "range": "± 2242365",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9609565,
            "range": "± 121001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 45733716,
            "range": "± 6890302",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9517186,
            "range": "± 413464",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81495593,
            "range": "± 11164956",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 520372112,
            "range": "± 14789027",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9503965,
            "range": "± 1333438",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 52672485,
            "range": "± 2467747",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 2752770,
            "range": "± 75132",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 11938598,
            "range": "± 174616",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 2731054,
            "range": "± 40643",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 11796065,
            "range": "± 204754",
            "unit": "ns/iter"
          }
        ]
      },
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
          "id": "4352ffdcd2fe8d7d394b605243a31cb5e74d9b33",
          "message": "Add IBM MQ support",
          "timestamp": "2026-01-06T21:06:25Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/9/commits/4352ffdcd2fe8d7d394b605243a31cb5e74d9b33"
        },
        "date": 1767803928016,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1287070165,
            "range": "± 40303228",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2837581640,
            "range": "± 34762418",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 203291689,
            "range": "± 364055892",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 314204198,
            "range": "± 3337623",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1463525804,
            "range": "± 234088094",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5138562,
            "range": "± 3173882",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 1067514680,
            "range": "± 48467863",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3392963,
            "range": "± 253500",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25190251,
            "range": "± 2924469",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20733773,
            "range": "± 3011357",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23414770,
            "range": "± 1727721",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19945613,
            "range": "± 2053942",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16957764,
            "range": "± 3965480",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9616497,
            "range": "± 229191",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 46143179,
            "range": "± 5829687",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8955444,
            "range": "± 318304",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80556410,
            "range": "± 10389329",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 522204558,
            "range": "± 11818082",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 10354819,
            "range": "± 1239754",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 53588017,
            "range": "± 2735036",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 2841157,
            "range": "± 68132",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 11990299,
            "range": "± 257810",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 2779639,
            "range": "± 51238",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 11924790,
            "range": "± 129511",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}