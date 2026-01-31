window.BENCHMARK_DATA = {
  "lastUpdate": 1769869174598,
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
          "id": "c9ec38fad069770ce8133e4237f0304a6625d52a",
          "message": "Merge pull request #9 from marcomq/dev\n\nAdd IBM MQ support",
          "timestamp": "2026-01-07T17:38:06+01:00",
          "tree_id": "0d5408c28e582864ece82594e36ce819538fc6d8",
          "url": "https://github.com/marcomq/mq-bridge/commit/c9ec38fad069770ce8133e4237f0304a6625d52a"
        },
        "date": 1767804790026,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1243127318,
            "range": "± 33495286",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2758931822,
            "range": "± 30389333",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 195450405,
            "range": "± 9383001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 303612392,
            "range": "± 11129091",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1466204489,
            "range": "± 231380122",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4752378,
            "range": "± 662915",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 1069432475,
            "range": "± 51294135",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2975291,
            "range": "± 352220",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24706030,
            "range": "± 2820406",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20139754,
            "range": "± 1736323",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22449223,
            "range": "± 1311637",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19386042,
            "range": "± 1270641",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15457482,
            "range": "± 3645642",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9613474,
            "range": "± 181836",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 44707924,
            "range": "± 5879931",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8981079,
            "range": "± 199371",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 85143666,
            "range": "± 9396399",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 503659416,
            "range": "± 24002683",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 10656660,
            "range": "± 1301413",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49094840,
            "range": "± 2661913",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 2753779,
            "range": "± 75054",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 11892400,
            "range": "± 135954",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 2758699,
            "range": "± 58572",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 11724350,
            "range": "± 112832",
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
          "id": "f1a7bfe7ade23786157c9b4a88c8d6d0af0790a6",
          "message": "Remove release archive creation from workflow\n\nRemoved the step for creating a release archive in the workflow.",
          "timestamp": "2026-01-07T22:43:40+01:00",
          "tree_id": "f3be619bb81bd56c2cb3a9ddefa1da475a599fc4",
          "url": "https://github.com/marcomq/mq-bridge/commit/f1a7bfe7ade23786157c9b4a88c8d6d0af0790a6"
        },
        "date": 1767823119330,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1293385747,
            "range": "± 33844766",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2781966465,
            "range": "± 23949366",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 197825964,
            "range": "± 144257039",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 310543887,
            "range": "± 11920421",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1908658786,
            "range": "± 234200280",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5022450,
            "range": "± 362803",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 1045404030,
            "range": "± 25734994",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3438880,
            "range": "± 958869",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 26621523,
            "range": "± 1916615",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20296774,
            "range": "± 2206223",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22454355,
            "range": "± 1761395",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19537491,
            "range": "± 2626400",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 17210609,
            "range": "± 3852648",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9600328,
            "range": "± 362999",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 45629310,
            "range": "± 4394319",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9050527,
            "range": "± 375055",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80772084,
            "range": "± 9596412",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 512946075,
            "range": "± 14306120",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 10851090,
            "range": "± 1260027",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49003455,
            "range": "± 2087040",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 2753696,
            "range": "± 57076",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 12019159,
            "range": "± 284600",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 2733076,
            "range": "± 65940",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 12121455,
            "range": "± 172358",
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
          "id": "c268cb9515517e977e8e2de47b69822b9fd5566b",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/c268cb9515517e977e8e2de47b69822b9fd5566b"
        },
        "date": 1767891886614,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1258159389,
            "range": "± 28142534",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2770156295,
            "range": "± 30694365",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 192863160,
            "range": "± 9073183",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 305348974,
            "range": "± 8777508",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1813162074,
            "range": "± 234875471",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5103611,
            "range": "± 922254",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902076010,
            "range": "± 455762",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3051868,
            "range": "± 376272",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24676065,
            "range": "± 2497516",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20026774,
            "range": "± 1642616",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22508440,
            "range": "± 2424991",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19268726,
            "range": "± 3383014",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15557292,
            "range": "± 2446828",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9464754,
            "range": "± 484454",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43500334,
            "range": "± 2458232",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9067322,
            "range": "± 314360",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80063250,
            "range": "± 8055055",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 563415027,
            "range": "± 48804089",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8733181,
            "range": "± 5110933",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38905424,
            "range": "± 1764653",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 996753,
            "range": "± 38444",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22543635,
            "range": "± 226418",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 999766,
            "range": "± 58636",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22471305,
            "range": "± 601255",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 1025916,
            "range": "± 43668",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2851508,
            "range": "± 37944",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 966742,
            "range": "± 21398",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2379802,
            "range": "± 77355",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 792640,
            "range": "± 38725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1266069,
            "range": "± 18559",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 742449,
            "range": "± 12849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 538623,
            "range": "± 36786",
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
          "id": "34b33a2052bd7ec687cb45937e71f5442886eaa6",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/34b33a2052bd7ec687cb45937e71f5442886eaa6"
        },
        "date": 1767902979007,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1279692658,
            "range": "± 41880707",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2783059221,
            "range": "± 39769349",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193900697,
            "range": "± 13657771",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 302430636,
            "range": "± 4842795",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1369567652,
            "range": "± 233211440",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4413698,
            "range": "± 1196390",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902776835,
            "range": "± 36219818",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2948810,
            "range": "± 266230",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25023815,
            "range": "± 2331227",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19645209,
            "range": "± 1778070",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21724182,
            "range": "± 1911909",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19450791,
            "range": "± 3814706",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15346513,
            "range": "± 2593747",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9313153,
            "range": "± 331620",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43067481,
            "range": "± 5448721",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8790209,
            "range": "± 278399",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80925466,
            "range": "± 10242964",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 537086004,
            "range": "± 45717521",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8009131,
            "range": "± 5351971",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39216861,
            "range": "± 4060474",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 915681,
            "range": "± 94503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22311367,
            "range": "± 291657",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 978743,
            "range": "± 66596",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22536443,
            "range": "± 444651",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 1033220,
            "range": "± 33331",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2848240,
            "range": "± 28290",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 992411,
            "range": "± 29947",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2284332,
            "range": "± 96220",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 800984,
            "range": "± 20699",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1360399,
            "range": "± 31725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 763140,
            "range": "± 26699",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 596019,
            "range": "± 48242",
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
          "id": "290e6ff2990cc03c6b68273d59d2621ac9f7a3a3",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/290e6ff2990cc03c6b68273d59d2621ac9f7a3a3"
        },
        "date": 1767906967273,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1344163833,
            "range": "± 37256573",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2847966093,
            "range": "± 37695232",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 199262176,
            "range": "± 11150436",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 319123989,
            "range": "± 7646458",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1369246193,
            "range": "± 142833817",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4946940,
            "range": "± 1962312",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 904410976,
            "range": "± 47811683",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3059264,
            "range": "± 371801",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24412349,
            "range": "± 3102229",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20371562,
            "range": "± 2342071",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21855701,
            "range": "± 3162310",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19130568,
            "range": "± 3003814",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14401840,
            "range": "± 3540006",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9511578,
            "range": "± 443879",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41226723,
            "range": "± 3492856",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9017285,
            "range": "± 552295",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79795901,
            "range": "± 8048211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 557811648,
            "range": "± 49735661",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8910664,
            "range": "± 6272562",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40051006,
            "range": "± 2222415",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 867232,
            "range": "± 53174",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22266818,
            "range": "± 422658",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 877731,
            "range": "± 36504",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22452168,
            "range": "± 197490",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 986046,
            "range": "± 33678",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2793634,
            "range": "± 33065",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 972062,
            "range": "± 47697",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2158647,
            "range": "± 57059",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 763325,
            "range": "± 18971",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1309320,
            "range": "± 15161",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 732120,
            "range": "± 12317",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 564619,
            "range": "± 55247",
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
          "id": "cb422918962454f501e61a66189d16548bd1595b",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/cb422918962454f501e61a66189d16548bd1595b"
        },
        "date": 1767908566115,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1291438940,
            "range": "± 34911958",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2784494792,
            "range": "± 36436258",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 192091170,
            "range": "± 9044575",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 306593226,
            "range": "± 8528247",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1366758855,
            "range": "± 234657367",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4390486,
            "range": "± 695683",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902974093,
            "range": "± 24170379",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2826115,
            "range": "± 152705",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24141903,
            "range": "± 2293170",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19889751,
            "range": "± 1997418",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21875738,
            "range": "± 1732319",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19394411,
            "range": "± 3707981",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14743779,
            "range": "± 2416295",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9478103,
            "range": "± 350804",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42248107,
            "range": "± 2908995",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8813284,
            "range": "± 279848",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81132064,
            "range": "± 10677518",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 543237583,
            "range": "± 41007323",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8528340,
            "range": "± 4752469",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39145312,
            "range": "± 1980988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 921419,
            "range": "± 77488",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22465188,
            "range": "± 211366",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 928300,
            "range": "± 31278",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22338227,
            "range": "± 239566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 997770,
            "range": "± 20989",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2764253,
            "range": "± 35424",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 956091,
            "range": "± 32107",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2151694,
            "range": "± 46694",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 752722,
            "range": "± 18911",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1264853,
            "range": "± 10273",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 713760,
            "range": "± 27039",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 536928,
            "range": "± 31717",
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
          "id": "1b8dd43cfb11f6153569cee0b6d41d7800425470",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/1b8dd43cfb11f6153569cee0b6d41d7800425470"
        },
        "date": 1767908688795,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1269334543,
            "range": "± 31775082",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2784774284,
            "range": "± 27710347",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 194025134,
            "range": "± 2659003",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 307309514,
            "range": "± 7157563",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1366758467,
            "range": "± 234106955",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4568013,
            "range": "± 594024",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 953604424,
            "range": "± 44059943",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2945453,
            "range": "± 266460",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24604716,
            "range": "± 1385799",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20243437,
            "range": "± 1706815",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22182577,
            "range": "± 2037701",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19486294,
            "range": "± 3143853",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15880025,
            "range": "± 4054604",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9360761,
            "range": "± 446943",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43623224,
            "range": "± 3438512",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8971873,
            "range": "± 382984",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80110862,
            "range": "± 8649248",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 572414338,
            "range": "± 32289932",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8702785,
            "range": "± 5112037",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39684552,
            "range": "± 4174058",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 960595,
            "range": "± 66082",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22949498,
            "range": "± 343060",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 966205,
            "range": "± 52505",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 23136136,
            "range": "± 233237",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 1064210,
            "range": "± 20434",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2973075,
            "range": "± 25442",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1007228,
            "range": "± 18402",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2440460,
            "range": "± 83639",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 811103,
            "range": "± 18572",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1313164,
            "range": "± 20866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 766724,
            "range": "± 22388",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 581020,
            "range": "± 36261",
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
          "id": "6cb674fc92a539d7ea47688c7bf2008f13867507",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/6cb674fc92a539d7ea47688c7bf2008f13867507"
        },
        "date": 1767908880013,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1283851933,
            "range": "± 29520701",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2770291984,
            "range": "± 30493924",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 191970432,
            "range": "± 8036846",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 304903399,
            "range": "± 2855235",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1373996609,
            "range": "± 231475447",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4626105,
            "range": "± 424790",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 928222913,
            "range": "± 49903288",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2848605,
            "range": "± 215472",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24000089,
            "range": "± 1518972",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20131882,
            "range": "± 1361622",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22309003,
            "range": "± 2673270",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19178908,
            "range": "± 3297144",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14262847,
            "range": "± 3837828",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9183431,
            "range": "± 223163",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42718870,
            "range": "± 5984765",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8820619,
            "range": "± 217206",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80786376,
            "range": "± 9632402",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 533624900,
            "range": "± 45905031",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8215576,
            "range": "± 4752087",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 41530558,
            "range": "± 1770493",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 869517,
            "range": "± 38847",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22384260,
            "range": "± 184309",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 933636,
            "range": "± 50199",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22571875,
            "range": "± 211827",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 995333,
            "range": "± 31150",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2908293,
            "range": "± 54078",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1037766,
            "range": "± 36166",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2369527,
            "range": "± 115766",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 831002,
            "range": "± 21723",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1445743,
            "range": "± 34327",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 833219,
            "range": "± 27362",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 606805,
            "range": "± 37129",
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
          "id": "366ac617f2f9b2d9b791e2f65eb2babd769071e5",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/366ac617f2f9b2d9b791e2f65eb2babd769071e5"
        },
        "date": 1767910270958,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1335149488,
            "range": "± 40149781",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2950807538,
            "range": "± 62784293",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 205876572,
            "range": "± 13036001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 323635499,
            "range": "± 4118844",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1378991105,
            "range": "± 231944682",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5161547,
            "range": "± 756776",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903038138,
            "range": "± 24538491",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3234686,
            "range": "± 366847",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25127991,
            "range": "± 2104557",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20168293,
            "range": "± 1510647",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22549764,
            "range": "± 3735035",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19534748,
            "range": "± 3075450",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16584110,
            "range": "± 5558096",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9610480,
            "range": "± 384501",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43158964,
            "range": "± 2578753",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9129771,
            "range": "± 398223",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82713469,
            "range": "± 11192837",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 550493487,
            "range": "± 29519079",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8551376,
            "range": "± 5149443",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40741554,
            "range": "± 1826249",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 851885,
            "range": "± 50278",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22650278,
            "range": "± 182719",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 864594,
            "range": "± 54212",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22557405,
            "range": "± 1565566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 1010343,
            "range": "± 20448",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2869617,
            "range": "± 53250",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 983163,
            "range": "± 40577",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2252678,
            "range": "± 75341",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 757182,
            "range": "± 42780",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1303500,
            "range": "± 22410",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 727318,
            "range": "± 10913",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 569837,
            "range": "± 28454",
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
          "id": "0631e2190bb843f2bec8c8f999fd3e2fa521d61c",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/0631e2190bb843f2bec8c8f999fd3e2fa521d61c"
        },
        "date": 1767911793811,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1311764235,
            "range": "± 41615334",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2837038029,
            "range": "± 32791553",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 196752013,
            "range": "± 2665935",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 310020469,
            "range": "± 2553263",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1372783902,
            "range": "± 234538517",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4993617,
            "range": "± 779161",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903505262,
            "range": "± 35502307",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3223429,
            "range": "± 371175",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24451440,
            "range": "± 1747312",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20256718,
            "range": "± 1555499",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22447397,
            "range": "± 3452940",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19614355,
            "range": "± 3557756",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14953567,
            "range": "± 3647206",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9489312,
            "range": "± 146819",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43927092,
            "range": "± 3152148",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8994018,
            "range": "± 294499",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80573385,
            "range": "± 8969715",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 556720803,
            "range": "± 34552288",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7856490,
            "range": "± 5064608",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40418254,
            "range": "± 3901631",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 867690,
            "range": "± 39293",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22637217,
            "range": "± 129964",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 912013,
            "range": "± 36393",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22474098,
            "range": "± 643004",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 998694,
            "range": "± 26451",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2817533,
            "range": "± 25570",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 970733,
            "range": "± 20985",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2216073,
            "range": "± 45912",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 762581,
            "range": "± 16326",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1276690,
            "range": "± 19374",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 733862,
            "range": "± 21242",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 535231,
            "range": "± 40468",
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
          "id": "5060cdfd1689377bd8e6be78a85c51cf5fe3ba4f",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/5060cdfd1689377bd8e6be78a85c51cf5fe3ba4f"
        },
        "date": 1767911933798,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1248855783,
            "range": "± 33753189",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2777697089,
            "range": "± 24492538",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 191644486,
            "range": "± 6938002",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 308920792,
            "range": "± 7049624",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1368474365,
            "range": "± 216990009",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4387872,
            "range": "± 356110",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903180937,
            "range": "± 42317991",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2931637,
            "range": "± 204832",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24746473,
            "range": "± 1646339",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20032624,
            "range": "± 1548862",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21975231,
            "range": "± 3160537",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19343858,
            "range": "± 3006265",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15876715,
            "range": "± 4391884",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9680957,
            "range": "± 395492",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42778464,
            "range": "± 3521950",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8859908,
            "range": "± 247966",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82930218,
            "range": "± 9124722",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 547156554,
            "range": "± 44308119",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8267814,
            "range": "± 4884468",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38819790,
            "range": "± 2420517",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 897062,
            "range": "± 66660",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22493173,
            "range": "± 134429",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 962405,
            "range": "± 36769",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22490789,
            "range": "± 274106",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 1076046,
            "range": "± 51388",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2902657,
            "range": "± 47325",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1039475,
            "range": "± 20996",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2314837,
            "range": "± 46618",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 801397,
            "range": "± 19954",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1278897,
            "range": "± 11133",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 747814,
            "range": "± 19071",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 542194,
            "range": "± 20607",
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
          "id": "22de152915a3b13ad240e35983ee08969e7f888d",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/22de152915a3b13ad240e35983ee08969e7f888d"
        },
        "date": 1767912548254,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1268034244,
            "range": "± 115050317",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2762955575,
            "range": "± 27015234",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 194118056,
            "range": "± 3393132",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 308023385,
            "range": "± 4423601",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1593090981,
            "range": "± 237345170",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5074207,
            "range": "± 617482",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902818863,
            "range": "± 41975062",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2941994,
            "range": "± 202047",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24338337,
            "range": "± 5406203",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19721002,
            "range": "± 1693736",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22397744,
            "range": "± 2313260",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 18878646,
            "range": "± 3781329",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 17172987,
            "range": "± 2470849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9330516,
            "range": "± 370651",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42775065,
            "range": "± 4499246",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8930414,
            "range": "± 464523",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79705774,
            "range": "± 8206503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 554131228,
            "range": "± 53866430",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8284712,
            "range": "± 5555520",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39318291,
            "range": "± 7209488",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 959623,
            "range": "± 60280",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22259222,
            "range": "± 189851",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 908898,
            "range": "± 49488",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22226668,
            "range": "± 302729",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 999424,
            "range": "± 21850",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2803466,
            "range": "± 45874",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 991903,
            "range": "± 45878",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2263890,
            "range": "± 51718",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 740602,
            "range": "± 11612",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1258166,
            "range": "± 19912",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 721900,
            "range": "± 24649",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 529831,
            "range": "± 42909",
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
          "id": "a7148bed808f60cb7f1da761c2245fb00152e85f",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/a7148bed808f60cb7f1da761c2245fb00152e85f"
        },
        "date": 1767914347066,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1346297737,
            "range": "± 47618732",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2961236806,
            "range": "± 112238221",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 203241524,
            "range": "± 15016413",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 314857767,
            "range": "± 9255999",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1598158925,
            "range": "± 238748591",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5276129,
            "range": "± 638755",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903875228,
            "range": "± 42035138",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3109424,
            "range": "± 474266",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25381739,
            "range": "± 3537102",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20885347,
            "range": "± 1343332",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23209754,
            "range": "± 3413584",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 20381415,
            "range": "± 2849823",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15343808,
            "range": "± 3482878",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9628903,
            "range": "± 339150",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42294672,
            "range": "± 2378101",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8125209,
            "range": "± 543249",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 87589320,
            "range": "± 11847927",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 554978164,
            "range": "± 21590613",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8844525,
            "range": "± 5210722",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40588289,
            "range": "± 3300880",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 939360,
            "range": "± 79918",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22415436,
            "range": "± 220449",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 854361,
            "range": "± 63819",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22576663,
            "range": "± 437811",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3657314,
            "range": "± 29741",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2773175,
            "range": "± 28647",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1061877,
            "range": "± 38518",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1955164,
            "range": "± 50345",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 738203,
            "range": "± 45976",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1264907,
            "range": "± 76043",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 737424,
            "range": "± 28254",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 504529,
            "range": "± 16986",
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
          "id": "6c2603b5023c0ec14d77c3efe497f3d59521805f",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/6c2603b5023c0ec14d77c3efe497f3d59521805f"
        },
        "date": 1767948246765,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1367396720,
            "range": "± 38047385",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2845747948,
            "range": "± 45673105",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 205332515,
            "range": "± 9648505",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 315019559,
            "range": "± 2830503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1370855955,
            "range": "± 217421612",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4607541,
            "range": "± 426562",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903140073,
            "range": "± 810970",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2805483,
            "range": "± 204106",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24854969,
            "range": "± 1641626",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19937862,
            "range": "± 1493834",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22230161,
            "range": "± 2002314",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19487309,
            "range": "± 3231093",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16359720,
            "range": "± 2022970",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9434438,
            "range": "± 312308",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42750103,
            "range": "± 6217062",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9256816,
            "range": "± 388594",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82538992,
            "range": "± 10564361",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 554908893,
            "range": "± 20211566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7796170,
            "range": "± 4714976",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40203808,
            "range": "± 3496200",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 866676,
            "range": "± 56076",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 23046209,
            "range": "± 195572",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 862466,
            "range": "± 61350",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 23053261,
            "range": "± 315043",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3714967,
            "range": "± 17943",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2799203,
            "range": "± 36272",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1103613,
            "range": "± 30644",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2063423,
            "range": "± 38231",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 746671,
            "range": "± 11750",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1268068,
            "range": "± 9478",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 726283,
            "range": "± 24550",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 527156,
            "range": "± 28310",
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
          "id": "dee652c9ddbde658832534304b9d7080018a7651",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/dee652c9ddbde658832534304b9d7080018a7651"
        },
        "date": 1767952382894,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1303149457,
            "range": "± 40472386",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2799539335,
            "range": "± 34066238",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 190163512,
            "range": "± 4896585",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 307873001,
            "range": "± 8016521",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1376033464,
            "range": "± 233421799",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4715245,
            "range": "± 310918",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 929605952,
            "range": "± 42064763",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3016701,
            "range": "± 841020",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24189399,
            "range": "± 1631838",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20211394,
            "range": "± 1399276",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21908005,
            "range": "± 2151922",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19218369,
            "range": "± 3586447",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15917448,
            "range": "± 2266455",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9656508,
            "range": "± 399344",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42562610,
            "range": "± 3165892",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9007438,
            "range": "± 214497",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79073714,
            "range": "± 7901886",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 550061550,
            "range": "± 39324731",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8213785,
            "range": "± 5213488",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39109314,
            "range": "± 1867008",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 874070,
            "range": "± 23921",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22734473,
            "range": "± 247356",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 935879,
            "range": "± 62526",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22606007,
            "range": "± 264174",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3664945,
            "range": "± 35080",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2796286,
            "range": "± 26219",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1082679,
            "range": "± 34066",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1994609,
            "range": "± 61327",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 756538,
            "range": "± 19404",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1283827,
            "range": "± 15914",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 727656,
            "range": "± 20767",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 580435,
            "range": "± 52415",
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
          "id": "3eaa29ff3ecdd156d7275ab95f4105f7f2f4c141",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/3eaa29ff3ecdd156d7275ab95f4105f7f2f4c141"
        },
        "date": 1767952454497,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1394511757,
            "range": "± 40191995",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2923485080,
            "range": "± 44325219",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 204376224,
            "range": "± 3218273",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 323250608,
            "range": "± 15094753",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1372830714,
            "range": "± 143176542",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5216814,
            "range": "± 474738",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 906523475,
            "range": "± 51149114",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3338564,
            "range": "± 275819",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25600574,
            "range": "± 2728027",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20643357,
            "range": "± 2485774",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22846856,
            "range": "± 1821589",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19840623,
            "range": "± 3488930",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15842887,
            "range": "± 2822014",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9732006,
            "range": "± 209984",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 44461072,
            "range": "± 5416527",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9231037,
            "range": "± 328894",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82418862,
            "range": "± 8888835",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 579401885,
            "range": "± 30319219",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8572782,
            "range": "± 6424236",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 43882955,
            "range": "± 4188704",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 891454,
            "range": "± 43364",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 23181984,
            "range": "± 211795",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 857194,
            "range": "± 59021",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 23119448,
            "range": "± 447881",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3752316,
            "range": "± 28920",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2885441,
            "range": "± 24965",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1048516,
            "range": "± 49420",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2073620,
            "range": "± 85312",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 756079,
            "range": "± 8969",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1341337,
            "range": "± 14932",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 747874,
            "range": "± 21329",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 581231,
            "range": "± 39285",
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
          "id": "60a0668a03d708b7128920784b067ef3a3ce399b",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/60a0668a03d708b7128920784b067ef3a3ce399b"
        },
        "date": 1767953358661,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1333509194,
            "range": "± 63268095",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2953593056,
            "range": "± 46147618",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 207588975,
            "range": "± 15489763",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 330391864,
            "range": "± 9535585",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1818085269,
            "range": "± 231338205",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4823294,
            "range": "± 1450188",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903287466,
            "range": "± 35608156",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3251343,
            "range": "± 876830",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24826218,
            "range": "± 3315456",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20658975,
            "range": "± 1567542",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23101942,
            "range": "± 2955128",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19452946,
            "range": "± 2966391",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15961682,
            "range": "± 2234735",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9347049,
            "range": "± 243195",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43273772,
            "range": "± 4672565",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8577741,
            "range": "± 253477",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79169089,
            "range": "± 8032611",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 545892436,
            "range": "± 38774453",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8012189,
            "range": "± 4480986",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38703401,
            "range": "± 1630429",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 913825,
            "range": "± 38050",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22830727,
            "range": "± 189362",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 926610,
            "range": "± 50623",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22701445,
            "range": "± 265888",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3716555,
            "range": "± 52929",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2797545,
            "range": "± 27467",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1054991,
            "range": "± 50487",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1966114,
            "range": "± 56898",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 760037,
            "range": "± 19487",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1249774,
            "range": "± 38734",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 715642,
            "range": "± 25514",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 545703,
            "range": "± 43885",
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
          "id": "69b23aeb59eb0e6adc9abaeaa7d01faac5460a88",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/69b23aeb59eb0e6adc9abaeaa7d01faac5460a88"
        },
        "date": 1767953744688,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1295359522,
            "range": "± 41978045",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2806843423,
            "range": "± 42111978",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 196083464,
            "range": "± 9908857",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 307687006,
            "range": "± 8284442",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1592174850,
            "range": "± 238219667",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5112491,
            "range": "± 1352881",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902779484,
            "range": "± 42606169",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2957025,
            "range": "± 577258",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24314853,
            "range": "± 1669665",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20192356,
            "range": "± 1721653",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21822088,
            "range": "± 3117863",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19387964,
            "range": "± 3252866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15161811,
            "range": "± 3403186",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9242377,
            "range": "± 443266",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41853726,
            "range": "± 3273777",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8954382,
            "range": "± 397813",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 83592865,
            "range": "± 10337456",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 541491708,
            "range": "± 17310576",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9000642,
            "range": "± 4969165",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39414217,
            "range": "± 1659228",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 921949,
            "range": "± 57569",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22199538,
            "range": "± 167849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 955923,
            "range": "± 40123",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22428903,
            "range": "± 224008",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3661156,
            "range": "± 30709",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2758246,
            "range": "± 40725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1089062,
            "range": "± 39035",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1988194,
            "range": "± 77235",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 799952,
            "range": "± 26115",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1299918,
            "range": "± 23848",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 771499,
            "range": "± 34375",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 563915,
            "range": "± 43485",
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
          "id": "89762e47f154b7e199f1518d5e183c5a69f00ac4",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/89762e47f154b7e199f1518d5e183c5a69f00ac4"
        },
        "date": 1767954173620,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1193168642,
            "range": "± 33718260",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2616140933,
            "range": "± 33504725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 172740347,
            "range": "± 9185460",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 285717260,
            "range": "± 8202061",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1816494276,
            "range": "± 231903667",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4969003,
            "range": "± 658769",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 954558320,
            "range": "± 52217195",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2900530,
            "range": "± 71131",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 21103823,
            "range": "± 1258970",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 16889057,
            "range": "± 2353076",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 16970153,
            "range": "± 1387944",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 16047602,
            "range": "± 1937074",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 11453684,
            "range": "± 4012739",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 7711608,
            "range": "± 307415",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 28369219,
            "range": "± 7493868",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7627756,
            "range": "± 173915",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 62975494,
            "range": "± 4555166",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 506435912,
            "range": "± 39582770",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7502603,
            "range": "± 4216181",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39241837,
            "range": "± 2816029",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 815762,
            "range": "± 64557",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 11332291,
            "range": "± 204152",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 789554,
            "range": "± 24235",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 11357188,
            "range": "± 122991",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 2810633,
            "range": "± 35984",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2806667,
            "range": "± 51090",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 808938,
            "range": "± 28731",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1554131,
            "range": "± 60386",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 705089,
            "range": "± 22178",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1627892,
            "range": "± 30372",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 725501,
            "range": "± 52919",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 740950,
            "range": "± 34829",
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
          "id": "277cb0e2860eb53bce15ae44f5957537f1bdd23a",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/277cb0e2860eb53bce15ae44f5957537f1bdd23a"
        },
        "date": 1767954983406,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1305219561,
            "range": "± 38724098",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2792157562,
            "range": "± 40618723",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 195428606,
            "range": "± 9632671",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 316463471,
            "range": "± 3729120",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1370162688,
            "range": "± 218653023",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5106292,
            "range": "± 518149",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903432992,
            "range": "± 42578025",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3037265,
            "range": "± 407954",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24499583,
            "range": "± 1883868",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19949339,
            "range": "± 2952662",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22325073,
            "range": "± 1375216",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19656239,
            "range": "± 4170545",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14091774,
            "range": "± 2268292",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9471523,
            "range": "± 234749",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 40780309,
            "range": "± 4637764",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9006343,
            "range": "± 312353",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79270230,
            "range": "± 7934540",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 565822025,
            "range": "± 45374212",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8277886,
            "range": "± 4821111",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40847307,
            "range": "± 2119290",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 901339,
            "range": "± 57219",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22702904,
            "range": "± 232703",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 886949,
            "range": "± 58949",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22430183,
            "range": "± 270862",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3670180,
            "range": "± 35917",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2810080,
            "range": "± 14659",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1070487,
            "range": "± 36544",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1981800,
            "range": "± 85043",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 775969,
            "range": "± 28557",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1290348,
            "range": "± 15718",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 747127,
            "range": "± 20803",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 540849,
            "range": "± 34222",
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
          "id": "1c10653407e8b22030766a528a28d0edf63b67ef",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-07T21:43:44Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/1c10653407e8b22030766a528a28d0edf63b67ef"
        },
        "date": 1767955430012,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1264499939,
            "range": "± 43408589",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2788713616,
            "range": "± 32408012",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 194160585,
            "range": "± 2814139",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 305858248,
            "range": "± 8496125",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1588673469,
            "range": "± 239636339",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4828085,
            "range": "± 1603114",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 930766404,
            "range": "± 44957674",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3103749,
            "range": "± 316750",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23664398,
            "range": "± 5108295",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20083881,
            "range": "± 1242567",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22538938,
            "range": "± 3624669",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19273788,
            "range": "± 3193857",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16843122,
            "range": "± 3465932",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9663182,
            "range": "± 341657",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41747909,
            "range": "± 2010869",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9066701,
            "range": "± 314006",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 84187319,
            "range": "± 10611505",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 538289430,
            "range": "± 22696664",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8000649,
            "range": "± 4488230",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39551888,
            "range": "± 2618313",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 909082,
            "range": "± 44982",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22305643,
            "range": "± 203295",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 924515,
            "range": "± 42599",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22177763,
            "range": "± 170581",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3684037,
            "range": "± 36056",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2796343,
            "range": "± 58249",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1040351,
            "range": "± 31369",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1975243,
            "range": "± 29505",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 769580,
            "range": "± 11084",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1279376,
            "range": "± 11549",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 712140,
            "range": "± 18486",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 517917,
            "range": "± 26402",
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
          "id": "af1c9b81e73a461162109333e1c9ce19b4b8b02f",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-09T10:28:55Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/af1c9b81e73a461162109333e1c9ce19b4b8b02f"
        },
        "date": 1767957798474,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1297266330,
            "range": "± 32442159",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2792538524,
            "range": "± 32888700",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193021341,
            "range": "± 10835168",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 310786008,
            "range": "± 8066197",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1818750268,
            "range": "± 217584925",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4853389,
            "range": "± 962229",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 928830350,
            "range": "± 49627207",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3089596,
            "range": "± 224575",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24465432,
            "range": "± 1237096",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19589067,
            "range": "± 1729238",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21443200,
            "range": "± 2762441",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19250947,
            "range": "± 3001176",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15979751,
            "range": "± 3687994",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9586864,
            "range": "± 390121",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43883559,
            "range": "± 5890762",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9060921,
            "range": "± 352227",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81410782,
            "range": "± 9755149",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 548721199,
            "range": "± 47883133",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7761081,
            "range": "± 4671910",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39165073,
            "range": "± 2583596",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 909997,
            "range": "± 67188",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22982081,
            "range": "± 279946",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 946867,
            "range": "± 38009",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 23119956,
            "range": "± 535700",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3707112,
            "range": "± 25690",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2868941,
            "range": "± 24494",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1076600,
            "range": "± 56065",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1994990,
            "range": "± 42373",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 774540,
            "range": "± 23798",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1327355,
            "range": "± 25911",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 761563,
            "range": "± 20894",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 567264,
            "range": "± 42369",
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
          "id": "80a2800c446f20ea8cf00fb22ce587b0be2db874",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-09T10:28:55Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/80a2800c446f20ea8cf00fb22ce587b0be2db874"
        },
        "date": 1767958801462,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1284460139,
            "range": "± 35828653",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2692599734,
            "range": "± 50258593",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 181099307,
            "range": "± 8972988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 299264206,
            "range": "± 3305151",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1372436527,
            "range": "± 232865238",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5258701,
            "range": "± 621415",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903764478,
            "range": "± 51553457",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3233535,
            "range": "± 135366",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 21793356,
            "range": "± 1504761",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 17264270,
            "range": "± 1694283",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 17224310,
            "range": "± 1993124",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 16378199,
            "range": "± 1732728",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 13385145,
            "range": "± 4554974",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 8042793,
            "range": "± 250302",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 31081289,
            "range": "± 3340331",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7754615,
            "range": "± 134630",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 61912758,
            "range": "± 5898503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 507162513,
            "range": "± 37708755",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7645782,
            "range": "± 5871853",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40423188,
            "range": "± 2575258",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 821556,
            "range": "± 28159",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 11457637,
            "range": "± 170410",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 769851,
            "range": "± 30051",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 11425178,
            "range": "± 273365",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 2858039,
            "range": "± 46741",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2851495,
            "range": "± 48318",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 781937,
            "range": "± 29391",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1529425,
            "range": "± 79877",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 684685,
            "range": "± 31723",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1582102,
            "range": "± 16751",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 728343,
            "range": "± 28463",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 716484,
            "range": "± 49218",
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
          "id": "e1f5e0b06cf7d5da018c5cfa77361ab3d7756ce6",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-09T10:28:55Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/e1f5e0b06cf7d5da018c5cfa77361ab3d7756ce6"
        },
        "date": 1767959318723,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1332785281,
            "range": "± 40859885",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2804219241,
            "range": "± 44748283",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 197613642,
            "range": "± 10913726",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 318190392,
            "range": "± 4797929",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1820211657,
            "range": "± 190687510",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4970218,
            "range": "± 991809",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 929225916,
            "range": "± 50177551",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3110664,
            "range": "± 198104",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24787222,
            "range": "± 3232441",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20397475,
            "range": "± 1684100",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22529191,
            "range": "± 1676720",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19358465,
            "range": "± 3533413",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16845060,
            "range": "± 3586812",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9640104,
            "range": "± 431745",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41737675,
            "range": "± 3519606",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9173627,
            "range": "± 327724",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82418227,
            "range": "± 8212441",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 561652607,
            "range": "± 48512590",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9426243,
            "range": "± 5998721",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 41656928,
            "range": "± 2135415",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 880541,
            "range": "± 53273",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22507166,
            "range": "± 216596",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 880955,
            "range": "± 44785",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22483068,
            "range": "± 410607",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3679486,
            "range": "± 20156",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2732781,
            "range": "± 42873",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1084725,
            "range": "± 43307",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1907580,
            "range": "± 51861",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 754315,
            "range": "± 13648",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1294517,
            "range": "± 18188",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 727469,
            "range": "± 19288",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 561002,
            "range": "± 29953",
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
          "id": "80eb4133d1eb37ea6e0e983b338f79e8f311fa8b",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-09T10:28:55Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/80eb4133d1eb37ea6e0e983b338f79e8f311fa8b"
        },
        "date": 1767959878838,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1279415926,
            "range": "± 29387331",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2733705088,
            "range": "± 27310170",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 187807676,
            "range": "± 8914310",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 303107165,
            "range": "± 8902507",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1365136430,
            "range": "± 219523952",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5280518,
            "range": "± 774522",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902709102,
            "range": "± 42084086",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2857534,
            "range": "± 369082",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24042085,
            "range": "± 3858908",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19834856,
            "range": "± 1778248",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21949982,
            "range": "± 2894267",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19498217,
            "range": "± 3129357",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14497164,
            "range": "± 4256584",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9222394,
            "range": "± 378004",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 39969721,
            "range": "± 2636735",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8931963,
            "range": "± 365498",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81285440,
            "range": "± 10807220",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 534711052,
            "range": "± 28851347",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8561710,
            "range": "± 4857252",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39023005,
            "range": "± 2134598",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 906905,
            "range": "± 60210",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22303641,
            "range": "± 273630",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 916275,
            "range": "± 48439",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22505814,
            "range": "± 128942",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3645545,
            "range": "± 21435",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2748303,
            "range": "± 26922",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1063512,
            "range": "± 20727",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1974210,
            "range": "± 71345",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 759397,
            "range": "± 19395",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1276123,
            "range": "± 8324",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 722982,
            "range": "± 15229",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 549798,
            "range": "± 34370",
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
          "id": "60713a8fb7c5499399f66a6b6c0ac58e24b01589",
          "message": "Add ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-09T10:28:55Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/10/commits/60713a8fb7c5499399f66a6b6c0ac58e24b01589"
        },
        "date": 1767960039036,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1266037574,
            "range": "± 31220729",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2791825139,
            "range": "± 28242500",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 194299057,
            "range": "± 7667151",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 308557059,
            "range": "± 3071361",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1366667185,
            "range": "± 221975437",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4849885,
            "range": "± 750026",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903526543,
            "range": "± 47501334",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3142926,
            "range": "± 352345",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24004010,
            "range": "± 4392479",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20151598,
            "range": "± 1708169",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22391664,
            "range": "± 2444480",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19273506,
            "range": "± 3671905",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 17140207,
            "range": "± 2585738",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9372490,
            "range": "± 322692",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42429609,
            "range": "± 2847010",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8819512,
            "range": "± 491647",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 83111387,
            "range": "± 11010003",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 549250444,
            "range": "± 69231769",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7791143,
            "range": "± 4702362",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40007819,
            "range": "± 3449302",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 899719,
            "range": "± 36971",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22971729,
            "range": "± 275879",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 902729,
            "range": "± 64139",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 23160645,
            "range": "± 695055",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3757180,
            "range": "± 23871",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2943352,
            "range": "± 54218",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1131039,
            "range": "± 41071",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2113622,
            "range": "± 55471",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 785295,
            "range": "± 25420",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1307713,
            "range": "± 18782",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 754764,
            "range": "± 24442",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 593418,
            "range": "± 46143",
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
          "id": "537ba9d02a9dc4ef10ec63eb2e84e031973fe67c",
          "message": "Merge pull request #10 from marcomq/dev\n\nAdd ZeroMq support, change default batch size to 1 and stability fixes",
          "timestamp": "2026-01-09T14:30:02+01:00",
          "tree_id": "6019800b9c676863a5767fee6f7c90b27a1bbf09",
          "url": "https://github.com/marcomq/mq-bridge/commit/537ba9d02a9dc4ef10ec63eb2e84e031973fe67c"
        },
        "date": 1767966346175,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1312476252,
            "range": "± 40185854",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2828538976,
            "range": "± 32440164",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 194363460,
            "range": "± 3293674",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 309865995,
            "range": "± 8578360",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1601842471,
            "range": "± 234241342",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4911686,
            "range": "± 328106",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903990146,
            "range": "± 33525840",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3024033,
            "range": "± 232496",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24284523,
            "range": "± 2694478",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20207059,
            "range": "± 1453253",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22100282,
            "range": "± 2339220",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19721567,
            "range": "± 2839756",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14723552,
            "range": "± 3468139",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9598397,
            "range": "± 235527",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42355127,
            "range": "± 4923342",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9034142,
            "range": "± 151073",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81016579,
            "range": "± 8795498",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 552278927,
            "range": "± 26360437",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8090667,
            "range": "± 5491016",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39954293,
            "range": "± 3407819",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 888936,
            "range": "± 32583",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22678129,
            "range": "± 246426",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 890076,
            "range": "± 30101",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22664275,
            "range": "± 446315",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3700312,
            "range": "± 34891",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2825532,
            "range": "± 40855",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1086970,
            "range": "± 38914",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2045756,
            "range": "± 55200",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 771119,
            "range": "± 17363",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1319196,
            "range": "± 15200",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 739648,
            "range": "± 17906",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 560722,
            "range": "± 38958",
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
          "id": "4801b07de264b5c5a3021f61f782008ac2398013",
          "message": "remove config & async-stream dependencies",
          "timestamp": "2026-01-09T13:30:09Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/11/commits/4801b07de264b5c5a3021f61f782008ac2398013"
        },
        "date": 1768233201113,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1239695890,
            "range": "± 39603290",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2742176404,
            "range": "± 30344323",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 187597517,
            "range": "± 10700932",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 300440328,
            "range": "± 2219303",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1589056867,
            "range": "± 238535071",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4835141,
            "range": "± 2595454",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902874268,
            "range": "± 42257730",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2874087,
            "range": "± 179239",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24766508,
            "range": "± 3332861",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19532880,
            "range": "± 1397064",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21756258,
            "range": "± 2053843",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19014330,
            "range": "± 2776457",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15253982,
            "range": "± 2511213",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9112908,
            "range": "± 309219",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41791447,
            "range": "± 2413813",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8833485,
            "range": "± 276161",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78007414,
            "range": "± 10536122",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 550093299,
            "range": "± 42937264",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8432436,
            "range": "± 5101215",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38690453,
            "range": "± 2826878",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 955157,
            "range": "± 52474",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22491516,
            "range": "± 159785",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 909802,
            "range": "± 38426",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22528959,
            "range": "± 171001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3738183,
            "range": "± 41637",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2835104,
            "range": "± 27363",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1125992,
            "range": "± 31508",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2008121,
            "range": "± 53715",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 814520,
            "range": "± 25237",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1360127,
            "range": "± 15059",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 788466,
            "range": "± 17892",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 594304,
            "range": "± 34810",
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
          "id": "c46899ebf3dd9f151ca44ea7897d6a454e5d2b89",
          "message": "Merge pull request #11 from marcomq/dev\n\nremove config & async-stream dependencies",
          "timestamp": "2026-01-12T16:55:27+01:00",
          "tree_id": "3db25cddbd4492397ce61ac6c4e8a1d77ac21268",
          "url": "https://github.com/marcomq/mq-bridge/commit/c46899ebf3dd9f151ca44ea7897d6a454e5d2b89"
        },
        "date": 1768234257923,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1248020987,
            "range": "± 30108650",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2731649883,
            "range": "± 25170275",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 188470629,
            "range": "± 7311846",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 301290853,
            "range": "± 3944714",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1812059708,
            "range": "± 233956600",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4812343,
            "range": "± 414974",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 927832883,
            "range": "± 45989172",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2827859,
            "range": "± 335470",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23760061,
            "range": "± 1416241",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20919405,
            "range": "± 1594772",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21831630,
            "range": "± 3587282",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19013781,
            "range": "± 2981543",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 13923294,
            "range": "± 3101463",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9039834,
            "range": "± 242821",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41244564,
            "range": "± 1598475",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8722211,
            "range": "± 123447",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82151480,
            "range": "± 10062243",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 528221351,
            "range": "± 22736471",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8373093,
            "range": "± 5243706",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38491394,
            "range": "± 1671624",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 911842,
            "range": "± 42290",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 22369298,
            "range": "± 163694",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 912227,
            "range": "± 70550",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 22511311,
            "range": "± 196490",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3717754,
            "range": "± 54655",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2812413,
            "range": "± 30261",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1113190,
            "range": "± 77093",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1985300,
            "range": "± 42694",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 789431,
            "range": "± 20825",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1345735,
            "range": "± 16462",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 790063,
            "range": "± 27843",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 598727,
            "range": "± 32688",
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
          "id": "140d76321a747d2937c60df0878aac69094ae2ea",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/140d76321a747d2937c60df0878aac69094ae2ea"
        },
        "date": 1768375127534,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1323532293,
            "range": "± 67507619",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2736890748,
            "range": "± 35420909",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193214451,
            "range": "± 7411503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 303652450,
            "range": "± 8776295",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1590942269,
            "range": "± 238319431",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4519915,
            "range": "± 674294",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 904296401,
            "range": "± 35182993",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3185814,
            "range": "± 445554",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24646370,
            "range": "± 1483400",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20315592,
            "range": "± 3125115",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21839939,
            "range": "± 2746743",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19512076,
            "range": "± 2977072",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 13924325,
            "range": "± 2271085",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9292383,
            "range": "± 319887",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41349453,
            "range": "± 4966002",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8861371,
            "range": "± 470939",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78424278,
            "range": "± 9525478",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 549840004,
            "range": "± 54568875",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9209241,
            "range": "± 6485422",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38822157,
            "range": "± 1789844",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 857745,
            "range": "± 48353",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25055528,
            "range": "± 332166",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 875717,
            "range": "± 60959",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26023001,
            "range": "± 1089927",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3633686,
            "range": "± 27497",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2692970,
            "range": "± 33276",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1112922,
            "range": "± 40400",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1911002,
            "range": "± 62178",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 779344,
            "range": "± 10171",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1262108,
            "range": "± 14915",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 744720,
            "range": "± 22060",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 516739,
            "range": "± 18866",
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
          "id": "3443c5f2521e8b9fa5a76f09205733f4d9a4c758",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/3443c5f2521e8b9fa5a76f09205733f4d9a4c758"
        },
        "date": 1768375145426,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1326490587,
            "range": "± 50190940",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2814649353,
            "range": "± 51078548",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 198265607,
            "range": "± 10081689",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 314876103,
            "range": "± 4174716",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1370706067,
            "range": "± 233233997",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5076714,
            "range": "± 503799",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 904833987,
            "range": "± 34862419",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3117287,
            "range": "± 450112",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25533552,
            "range": "± 4494532",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20610415,
            "range": "± 2202166",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23255451,
            "range": "± 3459464",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 20438852,
            "range": "± 3786753",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16582035,
            "range": "± 3238334",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9620875,
            "range": "± 179348",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42633855,
            "range": "± 5671352",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9052338,
            "range": "± 246893",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 84693539,
            "range": "± 8268211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 585831446,
            "range": "± 23147582",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7742651,
            "range": "± 4380593",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 42206268,
            "range": "± 4303804",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 835530,
            "range": "± 41714",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25864619,
            "range": "± 838770",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 794354,
            "range": "± 32114",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27542562,
            "range": "± 1666044",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3761814,
            "range": "± 49288",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2819834,
            "range": "± 22964",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1088336,
            "range": "± 47362",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2011335,
            "range": "± 40357",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 773313,
            "range": "± 23447",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1451910,
            "range": "± 18178",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 731293,
            "range": "± 15468",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 590520,
            "range": "± 49391",
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
          "id": "d955b58f4f3b0b8747fb377aa9aebd170027c9b0",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/d955b58f4f3b0b8747fb377aa9aebd170027c9b0"
        },
        "date": 1768375309619,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1283874901,
            "range": "± 36994810",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2793886151,
            "range": "± 35545514",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 195547720,
            "range": "± 11423389",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 313247657,
            "range": "± 5898389",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1365031249,
            "range": "± 220332593",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4973252,
            "range": "± 666950",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902315718,
            "range": "± 41765347",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2924829,
            "range": "± 143642",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24525377,
            "range": "± 1358721",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19811731,
            "range": "± 1389478",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22162263,
            "range": "± 1959517",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19282778,
            "range": "± 3249075",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14329212,
            "range": "± 3833495",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9391116,
            "range": "± 72710",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42374780,
            "range": "± 6841590",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8724241,
            "range": "± 308042",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 77983121,
            "range": "± 8463484",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 556713381,
            "range": "± 39246537",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8434958,
            "range": "± 3840029",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39174241,
            "range": "± 1176988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 872438,
            "range": "± 31205",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25481727,
            "range": "± 650246",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 847279,
            "range": "± 63141",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26968281,
            "range": "± 1018762",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3756424,
            "range": "± 21358",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2861766,
            "range": "± 38869",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1104535,
            "range": "± 28485",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2076732,
            "range": "± 51374",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 806056,
            "range": "± 16347",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1507054,
            "range": "± 31364",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 784929,
            "range": "± 24279",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 678856,
            "range": "± 41634",
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
          "id": "b350fc0c03b3393b3549e92f7479045e28fc589b",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/b350fc0c03b3393b3549e92f7479045e28fc589b"
        },
        "date": 1768375706611,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1300385264,
            "range": "± 31584315",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2784041673,
            "range": "± 28276612",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 191086358,
            "range": "± 4114228",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 308320418,
            "range": "± 2609479",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1596727696,
            "range": "± 235859737",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4950268,
            "range": "± 395875",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 904967806,
            "range": "± 47387588",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3069245,
            "range": "± 350348",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24500509,
            "range": "± 1594072",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20362960,
            "range": "± 2789004",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22249365,
            "range": "± 2576427",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19243528,
            "range": "± 3426999",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15308275,
            "range": "± 3509266",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9453146,
            "range": "± 268311",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41691949,
            "range": "± 4050318",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8869014,
            "range": "± 423931",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 83552601,
            "range": "± 9221390",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 567276144,
            "range": "± 41179105",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8826584,
            "range": "± 5655572",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40785931,
            "range": "± 2762556",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 831437,
            "range": "± 35272",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25117928,
            "range": "± 557761",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 831931,
            "range": "± 54660",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26559268,
            "range": "± 1169909",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3676465,
            "range": "± 53005",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2746458,
            "range": "± 33045",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1083373,
            "range": "± 44230",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1960878,
            "range": "± 55483",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 765418,
            "range": "± 16438",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1451058,
            "range": "± 36506",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 732617,
            "range": "± 16290",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 645975,
            "range": "± 27908",
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
          "id": "29e885f58031c3ad78028d6ad83ff7ae9b6cda40",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/29e885f58031c3ad78028d6ad83ff7ae9b6cda40"
        },
        "date": 1768426544532,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1258481342,
            "range": "± 28961648",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2747005324,
            "range": "± 28791052",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 192051951,
            "range": "± 4951138",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 303864738,
            "range": "± 2997640",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1813713639,
            "range": "± 233776256",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4764885,
            "range": "± 416100",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902318679,
            "range": "± 52071410",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2918612,
            "range": "± 197209",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24259417,
            "range": "± 3549962",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19577064,
            "range": "± 1426200",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22125083,
            "range": "± 2325905",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19103898,
            "range": "± 3169096",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14028036,
            "range": "± 1783568",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9331474,
            "range": "± 295423",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42628311,
            "range": "± 1636329",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8730949,
            "range": "± 266587",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 84072973,
            "range": "± 10268775",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 541783808,
            "range": "± 21978351",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8365651,
            "range": "± 4651288",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39303088,
            "range": "± 1633158",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 889356,
            "range": "± 33072",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25011461,
            "range": "± 700172",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 877117,
            "range": "± 51456",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26122803,
            "range": "± 1186311",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3663965,
            "range": "± 17531",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2775001,
            "range": "± 47121",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1086230,
            "range": "± 32141",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1971805,
            "range": "± 31791",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 814774,
            "range": "± 29132",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1472397,
            "range": "± 26052",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 775897,
            "range": "± 14948",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 628395,
            "range": "± 41384",
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
          "id": "3a939eada799fb2c3f502f54960374f1ed7a04d4",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/3a939eada799fb2c3f502f54960374f1ed7a04d4"
        },
        "date": 1768427499560,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1293296746,
            "range": "± 29401703",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2781195303,
            "range": "± 47044140",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193339252,
            "range": "± 11365672",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 310411357,
            "range": "± 9277644",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1593630086,
            "range": "± 239169873",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5251733,
            "range": "± 986033",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903018287,
            "range": "± 35399757",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3013576,
            "range": "± 275317",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23860217,
            "range": "± 4940884",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19609584,
            "range": "± 1614425",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21155205,
            "range": "± 3245372",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19273053,
            "range": "± 3022440",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15384188,
            "range": "± 3947633",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9442400,
            "range": "± 255522",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41107904,
            "range": "± 3736491",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8804837,
            "range": "± 185662",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79445456,
            "range": "± 8935211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 553014121,
            "range": "± 43079838",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 10853851,
            "range": "± 8816859",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39496383,
            "range": "± 3125456",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 844603,
            "range": "± 32800",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25162187,
            "range": "± 761396",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 828067,
            "range": "± 44007",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26256747,
            "range": "± 1111765",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3645488,
            "range": "± 24860",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2735250,
            "range": "± 31225",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1085225,
            "range": "± 38855",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1920210,
            "range": "± 87516",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 755470,
            "range": "± 21988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1434518,
            "range": "± 15616",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 725635,
            "range": "± 11327",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 616156,
            "range": "± 19644",
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
          "id": "21f853dfc9c31247e1327d81a2e1d9d8ec950580",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/21f853dfc9c31247e1327d81a2e1d9d8ec950580"
        },
        "date": 1768427820042,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1322651367,
            "range": "± 44077301",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2815047398,
            "range": "± 35451720",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 198581852,
            "range": "± 7543178",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 317715914,
            "range": "± 4205516",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1374509898,
            "range": "± 232989423",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5033167,
            "range": "± 704941",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903389310,
            "range": "± 47471861",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2950248,
            "range": "± 356921",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24877998,
            "range": "± 5421920",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20305053,
            "range": "± 2159189",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22737176,
            "range": "± 1931279",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19755276,
            "range": "± 3725503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16890366,
            "range": "± 3285440",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9747003,
            "range": "± 100824",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43278999,
            "range": "± 4918246",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9056877,
            "range": "± 357093",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 92782447,
            "range": "± 10722612",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 556153885,
            "range": "± 14792854",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8795743,
            "range": "± 5578625",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 41005349,
            "range": "± 4614715",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 847882,
            "range": "± 51859",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25611205,
            "range": "± 553920",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 807662,
            "range": "± 29978",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26909802,
            "range": "± 1532922",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3710924,
            "range": "± 21728",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2786927,
            "range": "± 44047",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1076006,
            "range": "± 31148",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1959262,
            "range": "± 85223",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 762746,
            "range": "± 11069",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1461927,
            "range": "± 11343",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 723775,
            "range": "± 10446",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 619730,
            "range": "± 30604",
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
          "id": "f297d3dd080335f41e3fa7cf98c653b7744f3a4b",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/f297d3dd080335f41e3fa7cf98c653b7744f3a4b"
        },
        "date": 1768428300149,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1262519954,
            "range": "± 27625653",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2739688218,
            "range": "± 35201371",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 191545842,
            "range": "± 8164996",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 301581826,
            "range": "± 8700745",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1376785092,
            "range": "± 230707325",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4609398,
            "range": "± 554371",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902602789,
            "range": "± 42428404",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2810062,
            "range": "± 172095",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23757046,
            "range": "± 4343251",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19670677,
            "range": "± 1326743",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22006427,
            "range": "± 2218097",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19007350,
            "range": "± 3332172",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14966884,
            "range": "± 2491001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9230932,
            "range": "± 311603",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 44017344,
            "range": "± 5719181",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8612351,
            "range": "± 210343",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81923918,
            "range": "± 11292650",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 525666470,
            "range": "± 22197265",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9095453,
            "range": "± 5927733",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38473360,
            "range": "± 1520221",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 867566,
            "range": "± 25308",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25118365,
            "range": "± 576218",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 862274,
            "range": "± 29008",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26373225,
            "range": "± 1246177",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3646262,
            "range": "± 25431",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2742490,
            "range": "± 38849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1124546,
            "range": "± 31465",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2005342,
            "range": "± 37166",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 804524,
            "range": "± 17200",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1444039,
            "range": "± 28531",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 771236,
            "range": "± 26527",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 660238,
            "range": "± 57973",
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
          "id": "602ca7bac96fe12ba99cf2423bb5eb88e04fdf62",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/602ca7bac96fe12ba99cf2423bb5eb88e04fdf62"
        },
        "date": 1768429463572,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1298049036,
            "range": "± 33390890",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2818681717,
            "range": "± 26719858",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 195395380,
            "range": "± 13446856",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 315024133,
            "range": "± 11695190",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1817560052,
            "range": "± 219529220",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4565046,
            "range": "± 439721",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902966297,
            "range": "± 42568196",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2956522,
            "range": "± 927592",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24428139,
            "range": "± 1486107",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20138469,
            "range": "± 1377144",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21890815,
            "range": "± 3570910",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19363550,
            "range": "± 3927137",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16755910,
            "range": "± 3480247",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9567495,
            "range": "± 477503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43581026,
            "range": "± 5825503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8961596,
            "range": "± 249212",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80843611,
            "range": "± 10731443",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 541804550,
            "range": "± 43104126",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8545515,
            "range": "± 5540973",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39365405,
            "range": "± 3019039",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 856408,
            "range": "± 38266",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25082555,
            "range": "± 703480",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 837344,
            "range": "± 33391",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26472093,
            "range": "± 1317866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3667774,
            "range": "± 21787",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2769570,
            "range": "± 37416",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1093924,
            "range": "± 35734",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1982237,
            "range": "± 77961",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 761789,
            "range": "± 22766",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1465498,
            "range": "± 24294",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 726834,
            "range": "± 18501",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 643023,
            "range": "± 36432",
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
          "id": "7b9ca5ada269263411d011fad1c0f1905d3e7a3a",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/7b9ca5ada269263411d011fad1c0f1905d3e7a3a"
        },
        "date": 1768494057574,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1271701745,
            "range": "± 41154666",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2775599897,
            "range": "± 32787022",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193237887,
            "range": "± 9726438",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 305416249,
            "range": "± 7108838",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1371510897,
            "range": "± 230709433",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4793601,
            "range": "± 482256",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902760478,
            "range": "± 42701915",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3071223,
            "range": "± 112462",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24604916,
            "range": "± 3251436",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19889642,
            "range": "± 1560010",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21756108,
            "range": "± 4530366",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19150161,
            "range": "± 2903441",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 17499112,
            "range": "± 3829688",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9396398,
            "range": "± 284677",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42444709,
            "range": "± 5809633",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8877193,
            "range": "± 249985",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79142113,
            "range": "± 9558467",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 565911322,
            "range": "± 46378855",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8400461,
            "range": "± 4976925",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39584732,
            "range": "± 1750815",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 832428,
            "range": "± 47745",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25203300,
            "range": "± 562739",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 824471,
            "range": "± 29925",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26584061,
            "range": "± 1065041",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3690070,
            "range": "± 17754",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2763429,
            "range": "± 25918",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1082231,
            "range": "± 31246",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1997746,
            "range": "± 66024",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 784731,
            "range": "± 21171",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1449305,
            "range": "± 19911",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 719047,
            "range": "± 23954",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 625907,
            "range": "± 44369",
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
          "id": "48ec3dd31c684d2433e4ae367ff678e3c3d917b9",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/48ec3dd31c684d2433e4ae367ff678e3c3d917b9"
        },
        "date": 1768497821225,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1314307253,
            "range": "± 32674922",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2801646076,
            "range": "± 34502960",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 196995871,
            "range": "± 8035917",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 313878006,
            "range": "± 10646730",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1817102059,
            "range": "± 234990670",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5089748,
            "range": "± 713339",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903243781,
            "range": "± 48258797",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3357305,
            "range": "± 505219",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24384808,
            "range": "± 1789271",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19701125,
            "range": "± 1583830",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22366356,
            "range": "± 2054575",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19657948,
            "range": "± 3543408",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15324907,
            "range": "± 2644198",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9467042,
            "range": "± 359185",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41497980,
            "range": "± 2646181",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8531773,
            "range": "± 355948",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82303550,
            "range": "± 13114464",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 545556642,
            "range": "± 18516467",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8068400,
            "range": "± 5285523",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40179434,
            "range": "± 2544413",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 867523,
            "range": "± 49991",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25503482,
            "range": "± 736176",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 839605,
            "range": "± 27747",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26263750,
            "range": "± 1516681",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3678016,
            "range": "± 43416",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2791183,
            "range": "± 37040",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1105787,
            "range": "± 27727",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2005472,
            "range": "± 84993",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 788829,
            "range": "± 35291",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1429694,
            "range": "± 25335",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 756098,
            "range": "± 31539",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 638051,
            "range": "± 52634",
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
          "id": "7b3690ac666ae8c4682c910eee38264625fc3cc1",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/7b3690ac666ae8c4682c910eee38264625fc3cc1"
        },
        "date": 1768509613462,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1451076206,
            "range": "± 62283637",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2943948461,
            "range": "± 53136638",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 203468991,
            "range": "± 7565604",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 328613380,
            "range": "± 11500614",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1816853477,
            "range": "± 217158620",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5551285,
            "range": "± 500989",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 954166947,
            "range": "± 43611686",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 4230435,
            "range": "± 449658",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24795358,
            "range": "± 1494226",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20258052,
            "range": "± 3381718",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22143282,
            "range": "± 1873663",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19906673,
            "range": "± 3839231",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 19188667,
            "range": "± 4258184",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9840506,
            "range": "± 325497",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 44413193,
            "range": "± 5893389",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9365634,
            "range": "± 254945",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78326072,
            "range": "± 9179879",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 581051419,
            "range": "± 37768702",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8870844,
            "range": "± 5880091",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40665790,
            "range": "± 2123293",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 922527,
            "range": "± 35987",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25742518,
            "range": "± 750721",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 908991,
            "range": "± 52904",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27214156,
            "range": "± 1632800",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3800923,
            "range": "± 51049",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 3100541,
            "range": "± 66297",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1189042,
            "range": "± 69888",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2272165,
            "range": "± 60001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 820539,
            "range": "± 46663",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1599019,
            "range": "± 33345",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 830828,
            "range": "± 25732",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 705902,
            "range": "± 50437",
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
          "id": "c79e8269511277ca5d4776f15f1c24f205c48d28",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/c79e8269511277ca5d4776f15f1c24f205c48d28"
        },
        "date": 1768511687372,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1275035167,
            "range": "± 29791779",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2792917566,
            "range": "± 31335236",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 190135813,
            "range": "± 5346968",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 305932110,
            "range": "± 7796453",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1594431732,
            "range": "± 237159174",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5014038,
            "range": "± 444663",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902880382,
            "range": "± 51850544",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2899331,
            "range": "± 383942",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24841003,
            "range": "± 2285339",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19510010,
            "range": "± 1671151",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21619500,
            "range": "± 2074693",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19223110,
            "range": "± 3015308",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14031306,
            "range": "± 4628143",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9375539,
            "range": "± 249720",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42065823,
            "range": "± 2817471",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8759019,
            "range": "± 310442",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81344088,
            "range": "± 9084139",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 543029424,
            "range": "± 25716815",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8697964,
            "range": "± 5564054",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39438452,
            "range": "± 1369777",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 856067,
            "range": "± 53211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 24979116,
            "range": "± 646066",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 853357,
            "range": "± 56682",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26251576,
            "range": "± 1238656",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3720808,
            "range": "± 29988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2754839,
            "range": "± 19254",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1123597,
            "range": "± 43528",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2028370,
            "range": "± 26220",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 808271,
            "range": "± 18414",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1488716,
            "range": "± 17789",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 772743,
            "range": "± 20373",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 654843,
            "range": "± 41751",
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
          "id": "14cc9ea800722dcc26e8e5225100811254025985",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/14cc9ea800722dcc26e8e5225100811254025985"
        },
        "date": 1768513752227,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1334089774,
            "range": "± 37925355",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2883955676,
            "range": "± 26559639",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 203048594,
            "range": "± 15998272",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 317800636,
            "range": "± 4236033",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1371499546,
            "range": "± 189989928",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5175142,
            "range": "± 1116820",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903103863,
            "range": "± 31780705",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3192937,
            "range": "± 343147",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24659649,
            "range": "± 4658681",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20180213,
            "range": "± 2952003",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22438008,
            "range": "± 3096829",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19736971,
            "range": "± 3160062",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15616667,
            "range": "± 2451997",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9445930,
            "range": "± 236856",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42116833,
            "range": "± 5966177",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8829273,
            "range": "± 561865",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79320351,
            "range": "± 8862458",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 562672527,
            "range": "± 49312988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8410404,
            "range": "± 5763771",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 41584124,
            "range": "± 3066120",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 840763,
            "range": "± 44987",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25722582,
            "range": "± 665725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 816057,
            "range": "± 28211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26938275,
            "range": "± 1610653",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3764189,
            "range": "± 27771",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2759857,
            "range": "± 27257",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1010800,
            "range": "± 57000",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1939766,
            "range": "± 73522",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 756371,
            "range": "± 25950",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1452560,
            "range": "± 10475",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 726327,
            "range": "± 18959",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 628605,
            "range": "± 39972",
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
          "id": "324d6d6b49a80b24d9a3de390f2505e686c3ae0a",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/324d6d6b49a80b24d9a3de390f2505e686c3ae0a"
        },
        "date": 1768551420350,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1260188649,
            "range": "± 36205122",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2803185616,
            "range": "± 21701412",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193682944,
            "range": "± 9538118",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 312849511,
            "range": "± 9778702",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1595766344,
            "range": "± 234850988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4696453,
            "range": "± 477098",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903347283,
            "range": "± 42384853",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3072608,
            "range": "± 386167",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24441047,
            "range": "± 1581515",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20489590,
            "range": "± 2357590",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21638203,
            "range": "± 2155827",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19400556,
            "range": "± 3676027",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14201244,
            "range": "± 3033367",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9395295,
            "range": "± 273868",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41965776,
            "range": "± 5830894",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8687529,
            "range": "± 356649",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 77828891,
            "range": "± 9035548",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 563295587,
            "range": "± 40654273",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8963398,
            "range": "± 4843297",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39161138,
            "range": "± 2421816",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 901902,
            "range": "± 41927",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25220002,
            "range": "± 490016",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 890363,
            "range": "± 45703",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26475685,
            "range": "± 1105523",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3710186,
            "range": "± 16404",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2900556,
            "range": "± 76352",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1139263,
            "range": "± 31491",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2128881,
            "range": "± 66891",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 826848,
            "range": "± 20551",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1541419,
            "range": "± 16456",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 796462,
            "range": "± 21909",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 697679,
            "range": "± 36849",
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
          "id": "3f182d52ae5acbaacf9d77fffa0071b95f90eb60",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/3f182d52ae5acbaacf9d77fffa0071b95f90eb60"
        },
        "date": 1768551488843,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1387168760,
            "range": "± 42893912",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2911470100,
            "range": "± 35901876",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 208619297,
            "range": "± 14098054",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 326433639,
            "range": "± 4110843",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1821969081,
            "range": "± 217952358",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4757986,
            "range": "± 1213724",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 953585247,
            "range": "± 39344814",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3051579,
            "range": "± 548682",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 26068825,
            "range": "± 2876499",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20194558,
            "range": "± 2128756",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22440808,
            "range": "± 2971961",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19841704,
            "range": "± 3235249",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16424730,
            "range": "± 2938653",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9645039,
            "range": "± 355261",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42570379,
            "range": "± 3303350",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9174925,
            "range": "± 322517",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80442109,
            "range": "± 7682930",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 589482973,
            "range": "± 41159536",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8229435,
            "range": "± 4829065",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 44031786,
            "range": "± 4220755",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 861572,
            "range": "± 31402",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25979411,
            "range": "± 500189",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 826761,
            "range": "± 38961",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27171651,
            "range": "± 2224273",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3732787,
            "range": "± 15163",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2864612,
            "range": "± 46634",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1052058,
            "range": "± 29059",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2027831,
            "range": "± 87518",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 762257,
            "range": "± 21428",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1503807,
            "range": "± 35527",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 742518,
            "range": "± 14943",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 674770,
            "range": "± 29629",
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
          "id": "a8685ace8151b63c4cb70500b8805b8cfe29a7db",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/a8685ace8151b63c4cb70500b8805b8cfe29a7db"
        },
        "date": 1768578886501,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1310986728,
            "range": "± 37188543",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2802750215,
            "range": "± 37091561",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 196682924,
            "range": "± 11924541",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 315608589,
            "range": "± 8262286",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1371773737,
            "range": "± 215414813",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4974986,
            "range": "± 1524774",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903025985,
            "range": "± 31497910",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2821819,
            "range": "± 474651",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24284876,
            "range": "± 1535997",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20093803,
            "range": "± 1248384",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22271015,
            "range": "± 3077085",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19530670,
            "range": "± 3318484",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15070963,
            "range": "± 1753776",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9379477,
            "range": "± 293081",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42597885,
            "range": "± 4068149",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8434673,
            "range": "± 402757",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80724494,
            "range": "± 10244671",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 552364260,
            "range": "± 20136400",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8558574,
            "range": "± 6304537",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39705068,
            "range": "± 3382512",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 861262,
            "range": "± 44642",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25249972,
            "range": "± 615946",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 836685,
            "range": "± 28441",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 25967028,
            "range": "± 1580449",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3699453,
            "range": "± 44001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2758452,
            "range": "± 34855",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1054012,
            "range": "± 34924",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2011011,
            "range": "± 56471",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 776397,
            "range": "± 18574",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1430426,
            "range": "± 32030",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 723183,
            "range": "± 21969",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 631540,
            "range": "± 34017",
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
          "id": "6d13df789ef2d83a1a9d3677af0b7b43d669b41c",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/6d13df789ef2d83a1a9d3677af0b7b43d669b41c"
        },
        "date": 1768651393595,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1179713366,
            "range": "± 30341549",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2623968684,
            "range": "± 26254902",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 175519552,
            "range": "± 10648815",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 289942148,
            "range": "± 7750921",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1370035442,
            "range": "± 233594444",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5223463,
            "range": "± 853779",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 1002485799,
            "range": "± 48389585",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2784801,
            "range": "± 151635",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 21418524,
            "range": "± 1368788",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 16941470,
            "range": "± 2811942",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 17084676,
            "range": "± 1665601",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 15993042,
            "range": "± 2369309",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 11992042,
            "range": "± 2206037",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 7855013,
            "range": "± 237875",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 30058177,
            "range": "± 2296658",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7412963,
            "range": "± 273930",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 61630977,
            "range": "± 4185382",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 520721333,
            "range": "± 37513670",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7300072,
            "range": "± 3905443",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39237967,
            "range": "± 1522714",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 839386,
            "range": "± 51036",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 14366105,
            "range": "± 425227",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 738619,
            "range": "± 35552",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 14744583,
            "range": "± 481080",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 2800708,
            "range": "± 72204",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2889622,
            "range": "± 96440",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 799199,
            "range": "± 39297",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1663581,
            "range": "± 98551",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 781619,
            "range": "± 68432",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1908177,
            "range": "± 63330",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 736472,
            "range": "± 56387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 914326,
            "range": "± 75304",
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
          "id": "d7c2b890e5e3e09ae81177511a28c04470a2c78b",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/d7c2b890e5e3e09ae81177511a28c04470a2c78b"
        },
        "date": 1768654469050,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1341612353,
            "range": "± 41811013",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2829098756,
            "range": "± 39930935",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 196389646,
            "range": "± 11527229",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 311389430,
            "range": "± 7091071",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1366586339,
            "range": "± 233172451",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4861012,
            "range": "± 311600",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 904060061,
            "range": "± 47408847",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2971268,
            "range": "± 139551",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24688377,
            "range": "± 2917879",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20375818,
            "range": "± 1232484",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22654794,
            "range": "± 1759103",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19295515,
            "range": "± 3389890",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15706252,
            "range": "± 1903155",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9428785,
            "range": "± 312703",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 44696320,
            "range": "± 2180926",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9131322,
            "range": "± 403975",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79995104,
            "range": "± 7498371",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 586012834,
            "range": "± 40132308",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8336835,
            "range": "± 5607037",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40262540,
            "range": "± 3593907",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 876279,
            "range": "± 39975",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25925877,
            "range": "± 1875296",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 868688,
            "range": "± 54566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26847214,
            "range": "± 1218426",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3729322,
            "range": "± 32411",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2817182,
            "range": "± 68878",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1056076,
            "range": "± 33071",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1945307,
            "range": "± 60876",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 763775,
            "range": "± 25395",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1457429,
            "range": "± 23088",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 753401,
            "range": "± 18750",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 668235,
            "range": "± 52886",
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
          "id": "39cff2a75d4a44b11dfa3277351f76b851ecc7c5",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/39cff2a75d4a44b11dfa3277351f76b851ecc7c5"
        },
        "date": 1768654552969,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1313325032,
            "range": "± 38789273",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2817661030,
            "range": "± 27847906",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 198944648,
            "range": "± 4901185",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 314973304,
            "range": "± 2961337",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1818372305,
            "range": "± 218784440",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4741042,
            "range": "± 238087",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902997251,
            "range": "± 47865903",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3032552,
            "range": "± 242882",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24364952,
            "range": "± 3011623",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20025743,
            "range": "± 1674945",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21911789,
            "range": "± 2031656",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19420251,
            "range": "± 3047718",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14481491,
            "range": "± 2857642",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9340529,
            "range": "± 386766",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 40826335,
            "range": "± 3665688",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8976888,
            "range": "± 519922",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79468858,
            "range": "± 7630973",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 573210830,
            "range": "± 43017040",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9103828,
            "range": "± 5216209",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40113436,
            "range": "± 3531208",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 847092,
            "range": "± 30815",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25402414,
            "range": "± 913193",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 868064,
            "range": "± 31054",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26584359,
            "range": "± 2143718",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3694557,
            "range": "± 19430",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2760787,
            "range": "± 25615",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1075948,
            "range": "± 60446",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1973979,
            "range": "± 58994",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 779047,
            "range": "± 21903",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1473725,
            "range": "± 19019",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 737729,
            "range": "± 24739",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 633923,
            "range": "± 52067",
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
          "id": "c4d5bae96a05bf06b1d6ec0ed8c000eabb6701a8",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/c4d5bae96a05bf06b1d6ec0ed8c000eabb6701a8"
        },
        "date": 1768654913538,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1313858559,
            "range": "± 50380646",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2773079615,
            "range": "± 36345395",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 196868484,
            "range": "± 8585292",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 309250640,
            "range": "± 3646096",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1818160434,
            "range": "± 232644219",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4993536,
            "range": "± 671152",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903212620,
            "range": "± 42325917",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3021811,
            "range": "± 446482",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24771890,
            "range": "± 2665772",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19520365,
            "range": "± 2211274",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21765290,
            "range": "± 2253784",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 18973590,
            "range": "± 2808276",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15489355,
            "range": "± 3515152",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9393412,
            "range": "± 399490",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42358196,
            "range": "± 6037449",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9175881,
            "range": "± 239478",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 85725502,
            "range": "± 12529950",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 560964435,
            "range": "± 26747419",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7704439,
            "range": "± 4455539",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 43103561,
            "range": "± 3519995",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 850767,
            "range": "± 43707",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25539194,
            "range": "± 608569",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 833979,
            "range": "± 45387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26843176,
            "range": "± 1272464",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3766858,
            "range": "± 47386",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2859363,
            "range": "± 41664",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1066386,
            "range": "± 34786",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2003784,
            "range": "± 74296",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 748164,
            "range": "± 23110",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1466744,
            "range": "± 23443",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 731532,
            "range": "± 26482",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 609765,
            "range": "± 39996",
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
          "id": "cee9d891258702fa967af23abef101c2d0618da5",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/cee9d891258702fa967af23abef101c2d0618da5"
        },
        "date": 1768673659709,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1281342991,
            "range": "± 35316858",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2850254538,
            "range": "± 24217200",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 196595346,
            "range": "± 4683719",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 317358628,
            "range": "± 8532750",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1600765098,
            "range": "± 235906807",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5111607,
            "range": "± 838145",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 928728929,
            "range": "± 45671999",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2877141,
            "range": "± 263011",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24713458,
            "range": "± 1396983",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19471993,
            "range": "± 3024465",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22241130,
            "range": "± 2043384",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19028442,
            "range": "± 3316999",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16417217,
            "range": "± 3457556",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9298632,
            "range": "± 276951",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41872606,
            "range": "± 1791207",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8747843,
            "range": "± 319980",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78715685,
            "range": "± 8541675",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 554645153,
            "range": "± 50903811",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8660140,
            "range": "± 5228918",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39043490,
            "range": "± 3386305",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 873486,
            "range": "± 37989",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25147950,
            "range": "± 299670",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 851165,
            "range": "± 36054",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26701997,
            "range": "± 1172143",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3696522,
            "range": "± 22635",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2745949,
            "range": "± 39909",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1110882,
            "range": "± 38961",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1940012,
            "range": "± 51412",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 799075,
            "range": "± 24055",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1488887,
            "range": "± 14593",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 764166,
            "range": "± 24911",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 697763,
            "range": "± 36133",
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
          "id": "54b3c182117855e784cdce9c050ae5bc0cce854b",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/54b3c182117855e784cdce9c050ae5bc0cce854b"
        },
        "date": 1768674332731,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1422543794,
            "range": "± 48625442",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2992737104,
            "range": "± 68760666",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 209297075,
            "range": "± 5912634",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 331982979,
            "range": "± 6362965",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1378170083,
            "range": "± 231909883",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5432881,
            "range": "± 1183443",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903094820,
            "range": "± 47909033",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3429032,
            "range": "± 277023",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25202094,
            "range": "± 4107121",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20181453,
            "range": "± 2228127",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22638172,
            "range": "± 2587860",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19476132,
            "range": "± 2984134",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15134237,
            "range": "± 1867428",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9907101,
            "range": "± 511387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 44632398,
            "range": "± 4919217",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9221154,
            "range": "± 300205",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82423150,
            "range": "± 6684629",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 588047963,
            "range": "± 40556969",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9107891,
            "range": "± 4710508",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 45557429,
            "range": "± 4123410",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 825939,
            "range": "± 27955",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 26226050,
            "range": "± 533988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 806022,
            "range": "± 52435",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27285441,
            "range": "± 2173539",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3761826,
            "range": "± 38352",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2837195,
            "range": "± 39537",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1023688,
            "range": "± 35099",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1876739,
            "range": "± 99856",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 734564,
            "range": "± 24850",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1477426,
            "range": "± 44836",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 717633,
            "range": "± 20100",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 633200,
            "range": "± 37902",
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
          "id": "520bd464d03cac51d6fd4e4f6054ea59bbc5d7d0",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/520bd464d03cac51d6fd4e4f6054ea59bbc5d7d0"
        },
        "date": 1768675987104,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1275802403,
            "range": "± 28642225",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2778103293,
            "range": "± 26497678",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193015184,
            "range": "± 5668305",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 311808334,
            "range": "± 3853453",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1371985687,
            "range": "± 233427806",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4632937,
            "range": "± 465302",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903793837,
            "range": "± 51847748",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3054011,
            "range": "± 407604",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23907487,
            "range": "± 1699005",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19711260,
            "range": "± 1491595",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21917785,
            "range": "± 2049920",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19673469,
            "range": "± 3298607",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16290449,
            "range": "± 5355452",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9326471,
            "range": "± 252450",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41075973,
            "range": "± 5798566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8882155,
            "range": "± 362147",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81701424,
            "range": "± 8720483",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 535298973,
            "range": "± 35916623",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9436798,
            "range": "± 7644343",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39559836,
            "range": "± 3865880",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 847757,
            "range": "± 20010",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25124035,
            "range": "± 501432",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 818530,
            "range": "± 39633",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26122957,
            "range": "± 1928055",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3644007,
            "range": "± 28799",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2678114,
            "range": "± 32758",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1041624,
            "range": "± 29656",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1885640,
            "range": "± 64362",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 754565,
            "range": "± 13348",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1402522,
            "range": "± 23657",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 727776,
            "range": "± 15234",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 604445,
            "range": "± 35474",
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
          "id": "052c562f655adb00f47380173b5d1ca718c50b59",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/052c562f655adb00f47380173b5d1ca718c50b59"
        },
        "date": 1768676789403,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1263857152,
            "range": "± 36300329",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2727942460,
            "range": "± 25815799",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193588649,
            "range": "± 8450041",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 302424936,
            "range": "± 2890603",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1813303778,
            "range": "± 232562832",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4725758,
            "range": "± 765283",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902657785,
            "range": "± 20987725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2918839,
            "range": "± 416539",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23589291,
            "range": "± 1600127",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19340555,
            "range": "± 2380784",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21699383,
            "range": "± 1368198",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 18984542,
            "range": "± 2928812",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14691832,
            "range": "± 3145961",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9352299,
            "range": "± 210521",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41302450,
            "range": "± 2206793",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8859896,
            "range": "± 320399",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80109410,
            "range": "± 7055487",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 547192543,
            "range": "± 38965752",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8925575,
            "range": "± 6485791",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38978967,
            "range": "± 3329566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 831784,
            "range": "± 45343",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 24956698,
            "range": "± 666146",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 849545,
            "range": "± 33383",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26056943,
            "range": "± 1561982",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3661063,
            "range": "± 32306",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2702515,
            "range": "± 20489",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1097450,
            "range": "± 27574",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1958486,
            "range": "± 68471",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 789117,
            "range": "± 22125",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1455620,
            "range": "± 29215",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 725368,
            "range": "± 31635",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 618311,
            "range": "± 28821",
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
          "id": "189e5b9d5b5aaba02fa17c92d3521f6be3eeef28",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/189e5b9d5b5aaba02fa17c92d3521f6be3eeef28"
        },
        "date": 1768677200589,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1256363979,
            "range": "± 34900539",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2753183592,
            "range": "± 57497395",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 190208899,
            "range": "± 7805209",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 304826971,
            "range": "± 2614561",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1812483615,
            "range": "± 218182839",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4518761,
            "range": "± 434924",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902993251,
            "range": "± 41927928",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2805804,
            "range": "± 335956",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23820826,
            "range": "± 1402286",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19220108,
            "range": "± 1413385",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21417521,
            "range": "± 2013871",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 18813742,
            "range": "± 3300018",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14875960,
            "range": "± 2968641",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9252041,
            "range": "± 472591",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42621679,
            "range": "± 5142139",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8819773,
            "range": "± 143711",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78545724,
            "range": "± 8607574",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 533784683,
            "range": "± 41368734",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7810242,
            "range": "± 4575019",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38580130,
            "range": "± 1696528",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 886645,
            "range": "± 41372",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25303196,
            "range": "± 1060849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 861407,
            "range": "± 39814",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26154504,
            "range": "± 1588135",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3719271,
            "range": "± 34885",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2809866,
            "range": "± 30060",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1101938,
            "range": "± 35599",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1994655,
            "range": "± 57865",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 806437,
            "range": "± 17565",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1484631,
            "range": "± 34958",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 776011,
            "range": "± 29915",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 658657,
            "range": "± 27388",
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
          "id": "d2df172b20a44422bf6c0d770abf194e39475e82",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/d2df172b20a44422bf6c0d770abf194e39475e82"
        },
        "date": 1768677330296,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1586866622,
            "range": "± 82695033",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 3169801380,
            "range": "± 216709788",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 214013634,
            "range": "± 9693945",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 330400290,
            "range": "± 4587642",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1376465594,
            "range": "± 231877057",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5031105,
            "range": "± 262368",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903570216,
            "range": "± 41724176",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3376863,
            "range": "± 259213",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25543832,
            "range": "± 3044290",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20875208,
            "range": "± 2054721",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23044081,
            "range": "± 2416924",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 20057531,
            "range": "± 3295212",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15540668,
            "range": "± 2763240",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9904803,
            "range": "± 225642",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42964261,
            "range": "± 4379484",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9259571,
            "range": "± 202090",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81408564,
            "range": "± 7443970",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 575321693,
            "range": "± 33042556",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7971755,
            "range": "± 5337205",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 44347843,
            "range": "± 4325084",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 836969,
            "range": "± 43050",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25773238,
            "range": "± 687750",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 782900,
            "range": "± 39248",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27130021,
            "range": "± 752480",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3802628,
            "range": "± 24603",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2811839,
            "range": "± 39687",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 987228,
            "range": "± 22409",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1916175,
            "range": "± 59845",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 744909,
            "range": "± 21396",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1503735,
            "range": "± 15683",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 748299,
            "range": "± 16553",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 673682,
            "range": "± 21366",
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
          "id": "f1418123c8ac03030216c86eabbab709873f9e7c",
          "message": "Fix ordering issues, add memory response",
          "timestamp": "2026-01-12T15:55:31Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/12/commits/f1418123c8ac03030216c86eabbab709873f9e7c"
        },
        "date": 1768679016619,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1296836229,
            "range": "± 39258180",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2845955660,
            "range": "± 25300360",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 200645926,
            "range": "± 9963460",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 320029489,
            "range": "± 3519486",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1369427279,
            "range": "± 234415066",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4728020,
            "range": "± 787854",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902749797,
            "range": "± 16045342",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3104110,
            "range": "± 432375",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24338940,
            "range": "± 3162042",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20430887,
            "range": "± 1786739",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21963974,
            "range": "± 1473982",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19355219,
            "range": "± 3003411",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15390057,
            "range": "± 7299189",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9445998,
            "range": "± 192202",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41905149,
            "range": "± 4275090",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9110809,
            "range": "± 304211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81011029,
            "range": "± 8054629",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 557103884,
            "range": "± 52420341",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8503755,
            "range": "± 6706702",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40573407,
            "range": "± 2588159",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 839664,
            "range": "± 30049",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25393810,
            "range": "± 552797",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 809835,
            "range": "± 31436",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26572404,
            "range": "± 1507039",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3645815,
            "range": "± 14728",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2742700,
            "range": "± 16050",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1063534,
            "range": "± 13500",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1934608,
            "range": "± 42070",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 756380,
            "range": "± 17423",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1424609,
            "range": "± 17253",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 710367,
            "range": "± 16453",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 608365,
            "range": "± 56584",
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
          "id": "c63ea375ebf28606324fc65375af013b93893da2",
          "message": "Merge pull request #12 from marcomq/dev\n\nFix ordering issues, add memory response",
          "timestamp": "2026-01-17T20:28:08+01:00",
          "tree_id": "b8a78b49260a748fa4041232f64ac217fb049bbf",
          "url": "https://github.com/marcomq/mq-bridge/commit/c63ea375ebf28606324fc65375af013b93893da2"
        },
        "date": 1768679110834,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1274315137,
            "range": "± 36235393",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2790092073,
            "range": "± 32870733",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 191520433,
            "range": "± 11020513",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 309823602,
            "range": "± 9093575",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1369630540,
            "range": "± 219528005",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4535867,
            "range": "± 1309367",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 953466468,
            "range": "± 40680660",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3018881,
            "range": "± 381326",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24128714,
            "range": "± 1590170",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19535404,
            "range": "± 2672304",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21269645,
            "range": "± 2943154",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19149300,
            "range": "± 2898376",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16545603,
            "range": "± 2746534",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9352014,
            "range": "± 281642",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41462254,
            "range": "± 3208392",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8951842,
            "range": "± 331564",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81690297,
            "range": "± 10048406",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 554853765,
            "range": "± 39631259",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 10458714,
            "range": "± 7430894",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39919945,
            "range": "± 2463074",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 822165,
            "range": "± 24458",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25230936,
            "range": "± 629130",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 810561,
            "range": "± 37849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26460626,
            "range": "± 1617847",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3672878,
            "range": "± 21725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2731081,
            "range": "± 46791",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1096886,
            "range": "± 33276",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1908635,
            "range": "± 60884",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 754838,
            "range": "± 11664",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1413884,
            "range": "± 11081",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 724817,
            "range": "± 14582",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 592574,
            "range": "± 40203",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "Mmengelkoch@gmx.de",
            "name": "marco.mengelkoch",
            "username": "marcomq"
          },
          "committer": {
            "email": "Mmengelkoch@gmx.de",
            "name": "marco.mengelkoch",
            "username": "marcomq"
          },
          "distinct": true,
          "id": "452d3bae98d4e4f5997001e57f755fa3f2fcb65c",
          "message": "Merge branch 'dev'",
          "timestamp": "2026-01-17T21:02:58+01:00",
          "tree_id": "c15eba68cf5d72faef310538f86d32e4d4e7493c",
          "url": "https://github.com/marcomq/mq-bridge/commit/452d3bae98d4e4f5997001e57f755fa3f2fcb65c"
        },
        "date": 1768681127955,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1302623572,
            "range": "± 39824173",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2788755683,
            "range": "± 29488587",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193108487,
            "range": "± 3532057",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 307061767,
            "range": "± 3009567",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1372477927,
            "range": "± 232373206",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4634320,
            "range": "± 519914",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 952996321,
            "range": "± 47247037",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2791407,
            "range": "± 189678",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24374109,
            "range": "± 1707551",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19624196,
            "range": "± 2003601",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22153016,
            "range": "± 1797785",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19007007,
            "range": "± 3324517",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 17162128,
            "range": "± 3647295",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9247165,
            "range": "± 518836",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 40485628,
            "range": "± 3779917",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8698246,
            "range": "± 244261",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78700833,
            "range": "± 8658629",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 539145897,
            "range": "± 40413564",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8837124,
            "range": "± 5359759",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39667567,
            "range": "± 1861812",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 828023,
            "range": "± 21852",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25073715,
            "range": "± 630798",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 829240,
            "range": "± 36496",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26046663,
            "range": "± 1545405",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3648573,
            "range": "± 21238",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2765372,
            "range": "± 23379",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1073602,
            "range": "± 39664",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1905773,
            "range": "± 43804",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 753979,
            "range": "± 20421",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1400254,
            "range": "± 22528",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 714712,
            "range": "± 20857",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 596501,
            "range": "± 21705",
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
          "id": "dcb378c359469a9b7546fb8640e3fadf2bb536b6",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/dcb378c359469a9b7546fb8640e3fadf2bb536b6"
        },
        "date": 1768765067934,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1305540313,
            "range": "± 41250052",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2844682326,
            "range": "± 41537101",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 205657975,
            "range": "± 4780541",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 320955875,
            "range": "± 3128096",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1370938021,
            "range": "± 233504755",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5114062,
            "range": "± 732723",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 928867573,
            "range": "± 49487917",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3043558,
            "range": "± 191522",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24740758,
            "range": "± 2979707",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 21921166,
            "range": "± 2019154",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22386217,
            "range": "± 2999377",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19481710,
            "range": "± 3306356",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16910192,
            "range": "± 3342605",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9464591,
            "range": "± 384109",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41717064,
            "range": "± 2974767",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8772720,
            "range": "± 417455",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79307759,
            "range": "± 8614423",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 557440551,
            "range": "± 53045619",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8401567,
            "range": "± 4718404",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40712071,
            "range": "± 2120889",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 827307,
            "range": "± 29816",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25610058,
            "range": "± 491934",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 852285,
            "range": "± 23961",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27031021,
            "range": "± 970605",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3707068,
            "range": "± 14365",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2741386,
            "range": "± 29192",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1098881,
            "range": "± 49980",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2005025,
            "range": "± 81062",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 756943,
            "range": "± 20056",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1442458,
            "range": "± 25773",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 739486,
            "range": "± 24743",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 598519,
            "range": "± 50028",
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
          "id": "ac5dba8ba6fe7e5294baa2b282ef1dd301a88c08",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/ac5dba8ba6fe7e5294baa2b282ef1dd301a88c08"
        },
        "date": 1768766968244,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1281394950,
            "range": "± 43055721",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2750421216,
            "range": "± 35170559",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193153860,
            "range": "± 7731387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 307355198,
            "range": "± 3913128",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1370251496,
            "range": "± 218715855",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4695704,
            "range": "± 352059",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902771307,
            "range": "± 33682871",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3141891,
            "range": "± 229890",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23674477,
            "range": "± 3266115",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19849004,
            "range": "± 2273843",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21785370,
            "range": "± 2551337",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19377952,
            "range": "± 3105816",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14728534,
            "range": "± 3161321",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9440585,
            "range": "± 452308",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41230010,
            "range": "± 6305286",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8823625,
            "range": "± 528350",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 83656495,
            "range": "± 10416196",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 534389402,
            "range": "± 19408681",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9624617,
            "range": "± 7819746",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39853482,
            "range": "± 2866583",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 854156,
            "range": "± 34571",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25006583,
            "range": "± 589013",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 821577,
            "range": "± 36342",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 25912512,
            "range": "± 1626504",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3657079,
            "range": "± 30707",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2746005,
            "range": "± 31464",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1100500,
            "range": "± 34597",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1905389,
            "range": "± 48232",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 789571,
            "range": "± 36792",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1483556,
            "range": "± 21543",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 712225,
            "range": "± 14092",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 605061,
            "range": "± 41075",
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
          "id": "d02c137ac727b1851e0f678afc4f3ba21bbe33d7",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/d02c137ac727b1851e0f678afc4f3ba21bbe33d7"
        },
        "date": 1768847716657,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1298445653,
            "range": "± 48933702",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2866197414,
            "range": "± 24524826",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 202533256,
            "range": "± 11798653",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 317260292,
            "range": "± 9138591",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1376271985,
            "range": "± 230815706",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5153967,
            "range": "± 779703",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903241219,
            "range": "± 47793427",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2965722,
            "range": "± 327599",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24599219,
            "range": "± 4775681",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20015335,
            "range": "± 1349572",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22751852,
            "range": "± 2310429",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19510526,
            "range": "± 3268229",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14892367,
            "range": "± 3219734",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9562700,
            "range": "± 231237",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41794619,
            "range": "± 2427890",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8892293,
            "range": "± 193954",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81473710,
            "range": "± 8959035",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 566805432,
            "range": "± 47200234",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7665732,
            "range": "± 4703085",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39775507,
            "range": "± 2733709",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 824696,
            "range": "± 30258",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25342814,
            "range": "± 551682",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 821209,
            "range": "± 26648",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26547459,
            "range": "± 1304444",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3701752,
            "range": "± 37321",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2803131,
            "range": "± 44226",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1079313,
            "range": "± 28561",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1971186,
            "range": "± 99795",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 760791,
            "range": "± 16008",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1441200,
            "range": "± 21705",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 723224,
            "range": "± 15365",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 608122,
            "range": "± 47186",
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
          "id": "9ab9dc19d36561d6e400340fbb809d07481a8863",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/9ab9dc19d36561d6e400340fbb809d07481a8863"
        },
        "date": 1768847750929,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1301100567,
            "range": "± 42525281",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2808206696,
            "range": "± 17782414",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 194963942,
            "range": "± 12367081",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 311575385,
            "range": "± 9145950",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1369737621,
            "range": "± 220199600",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4745899,
            "range": "± 694869",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903821258,
            "range": "± 51754209",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3215337,
            "range": "± 356110",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24665409,
            "range": "± 2672333",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20004424,
            "range": "± 1708555",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22960428,
            "range": "± 2317214",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19728262,
            "range": "± 2874099",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14993121,
            "range": "± 3676358",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9501013,
            "range": "± 197383",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 49673875,
            "range": "± 6010212",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8720043,
            "range": "± 614795",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 86985477,
            "range": "± 6508982",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 600547102,
            "range": "± 33200057",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8248510,
            "range": "± 5870921",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 42269809,
            "range": "± 4013890",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 847445,
            "range": "± 66736",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 26466661,
            "range": "± 593433",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 862675,
            "range": "± 53047",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27477765,
            "range": "± 2235659",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3721132,
            "range": "± 31650",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2989989,
            "range": "± 164487",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1155368,
            "range": "± 62415",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2065045,
            "range": "± 107357",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 979087,
            "range": "± 62092",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 2176760,
            "range": "± 90270",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 958826,
            "range": "± 55354",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 1033785,
            "range": "± 71678",
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
          "id": "c2f623f27126145bf7eeef0712f2244b2392e823",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/c2f623f27126145bf7eeef0712f2244b2392e823"
        },
        "date": 1768848478585,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1257892299,
            "range": "± 33040223",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2743952014,
            "range": "± 24173728",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 189274477,
            "range": "± 3446849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 303925928,
            "range": "± 2755293",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1369618518,
            "range": "± 233133791",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4748447,
            "range": "± 3026817",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903630187,
            "range": "± 47995991",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2911048,
            "range": "± 210245",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24347748,
            "range": "± 1409153",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19515376,
            "range": "± 1639678",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22135595,
            "range": "± 2216053",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19013477,
            "range": "± 3048810",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14383815,
            "range": "± 2450635",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9222264,
            "range": "± 346455",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41159647,
            "range": "± 2926225",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8794071,
            "range": "± 164214",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79973641,
            "range": "± 8355240",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 552001268,
            "range": "± 42914006",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9836332,
            "range": "± 6947600",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39077744,
            "range": "± 2232750",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 862659,
            "range": "± 41194",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 24558458,
            "range": "± 752550",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 872389,
            "range": "± 56390",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26010177,
            "range": "± 1319317",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3715351,
            "range": "± 51710",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2781954,
            "range": "± 31870",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1111207,
            "range": "± 44696",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1969578,
            "range": "± 46265",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 783583,
            "range": "± 24186",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1473673,
            "range": "± 29545",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 766951,
            "range": "± 24633",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 636955,
            "range": "± 35885",
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
          "id": "b0118baedea3ae02d9dc982b657fbb588173c8a4",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/b0118baedea3ae02d9dc982b657fbb588173c8a4"
        },
        "date": 1768852018126,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1356160417,
            "range": "± 38854673",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2907690229,
            "range": "± 24814964",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 212716029,
            "range": "± 10357830",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 324405806,
            "range": "± 4757710",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1377994485,
            "range": "± 232072451",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5196129,
            "range": "± 845077",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902908564,
            "range": "± 15733912",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2960654,
            "range": "± 143372",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24766947,
            "range": "± 2025200",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19901304,
            "range": "± 1718345",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23012265,
            "range": "± 2065879",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19776037,
            "range": "± 3125853",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14861498,
            "range": "± 4298855",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9507602,
            "range": "± 282049",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42803501,
            "range": "± 3095044",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8761898,
            "range": "± 364061",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80025360,
            "range": "± 7320210",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 579188550,
            "range": "± 38310835",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8815560,
            "range": "± 8024211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 39715724,
            "range": "± 4430954",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 865625,
            "range": "± 28098",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 26209526,
            "range": "± 951620",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 878627,
            "range": "± 27926",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27153132,
            "range": "± 2051859",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3693457,
            "range": "± 28374",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2877186,
            "range": "± 40414",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1075081,
            "range": "± 49407",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2181183,
            "range": "± 39350",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 747873,
            "range": "± 15307",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1460298,
            "range": "± 26727",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 751159,
            "range": "± 19352",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 742959,
            "range": "± 19489",
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
          "id": "0507cd4c45825cd3eac60fcc8e2f33e417253c58",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/0507cd4c45825cd3eac60fcc8e2f33e417253c58"
        },
        "date": 1768858931540,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1344562136,
            "range": "± 74638977",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2783030270,
            "range": "± 93504067",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 188654704,
            "range": "± 10741562",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 298896237,
            "range": "± 15286669",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1594889917,
            "range": "± 237152304",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5238110,
            "range": "± 2209399",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 904352806,
            "range": "± 47869373",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3104405,
            "range": "± 309420",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 22871750,
            "range": "± 1837434",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 16961056,
            "range": "± 1215210",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 17484770,
            "range": "± 1780664",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 16539750,
            "range": "± 1503528",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 19709400,
            "range": "± 5595717",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 7972868,
            "range": "± 492756",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 33261279,
            "range": "± 7595071",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7560720,
            "range": "± 297866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 63124842,
            "range": "± 5596582",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 534357647,
            "range": "± 40517778",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7457942,
            "range": "± 6193120",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40378184,
            "range": "± 3167312",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 842485,
            "range": "± 27702",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 14561056,
            "range": "± 762581",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 757212,
            "range": "± 50272",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 15177993,
            "range": "± 813775",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 2883760,
            "range": "± 84986",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2852571,
            "range": "± 64918",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 821646,
            "range": "± 52560",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1666818,
            "range": "± 43243",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 701906,
            "range": "± 17635",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1900271,
            "range": "± 38422",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 740498,
            "range": "± 18766",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 979044,
            "range": "± 42309",
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
          "id": "6df14bd66ed34b3902517a1b8fe808316cd9880f",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/6df14bd66ed34b3902517a1b8fe808316cd9880f"
        },
        "date": 1768937361441,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1290896300,
            "range": "± 29736335",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2834747447,
            "range": "± 32065628",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 198890118,
            "range": "± 10472368",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 314784298,
            "range": "± 8388937",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1365493770,
            "range": "± 189856897",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5008121,
            "range": "± 2922567",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902987932,
            "range": "± 35048464",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2915975,
            "range": "± 378846",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24485984,
            "range": "± 1429791",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19700454,
            "range": "± 1333988",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22230759,
            "range": "± 2423327",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19056465,
            "range": "± 3111593",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16965816,
            "range": "± 4550785",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9380693,
            "range": "± 223737",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 42841983,
            "range": "± 2751346",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8649559,
            "range": "± 350916",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81269252,
            "range": "± 11580195",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 561111886,
            "range": "± 33621270",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8332633,
            "range": "± 5569343",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40901851,
            "range": "± 2105216",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 822997,
            "range": "± 25792",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25606214,
            "range": "± 692549",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 800123,
            "range": "± 39586",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26802457,
            "range": "± 1003208",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3649838,
            "range": "± 17913",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2801477,
            "range": "± 44417",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1100982,
            "range": "± 22065",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2194368,
            "range": "± 49841",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 773129,
            "range": "± 21725",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1404739,
            "range": "± 21351",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 730155,
            "range": "± 21955",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 702497,
            "range": "± 40696",
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
          "id": "0d0551eba816b33c23bfd50845014e486e0265da",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/0d0551eba816b33c23bfd50845014e486e0265da"
        },
        "date": 1768939145047,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1245887553,
            "range": "± 30124619",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2795564614,
            "range": "± 25685505",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 196397878,
            "range": "± 10069221",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 308732273,
            "range": "± 3562606",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1593760068,
            "range": "± 237660582",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4821993,
            "range": "± 716144",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902308736,
            "range": "± 42875151",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2885459,
            "range": "± 208999",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24785899,
            "range": "± 2543049",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19970858,
            "range": "± 1290940",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 23001810,
            "range": "± 2663123",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19644122,
            "range": "± 3048603",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15597319,
            "range": "± 2819853",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9108863,
            "range": "± 278795",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 43915979,
            "range": "± 3448576",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8785586,
            "range": "± 300351",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78481978,
            "range": "± 8231612",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 571223302,
            "range": "± 38916133",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8032521,
            "range": "± 5047307",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 38633063,
            "range": "± 2843190",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 856561,
            "range": "± 25959",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25331982,
            "range": "± 302873",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 897634,
            "range": "± 45351",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26575205,
            "range": "± 1160498",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3719117,
            "range": "± 29235",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2957104,
            "range": "± 47951",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1101933,
            "range": "± 37864",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2265675,
            "range": "± 42537",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 812977,
            "range": "± 24043",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1513894,
            "range": "± 18559",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 783837,
            "range": "± 31250",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 799170,
            "range": "± 36200",
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
          "id": "715d543f0a05e90238a8fb38991a29c11f2842f4",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/715d543f0a05e90238a8fb38991a29c11f2842f4"
        },
        "date": 1768978853320,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1333349513,
            "range": "± 44612906",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2923505721,
            "range": "± 46594720",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 202867265,
            "range": "± 5487855",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 318456260,
            "range": "± 3976028",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1373283588,
            "range": "± 187536931",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4792282,
            "range": "± 404706",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 928574904,
            "range": "± 50040109",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3045387,
            "range": "± 230484",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24884966,
            "range": "± 3156805",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20274494,
            "range": "± 2254370",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22253224,
            "range": "± 1714911",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19443024,
            "range": "± 3319488",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15767179,
            "range": "± 3090302",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9549696,
            "range": "± 281668",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 44673621,
            "range": "± 5930880",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9055869,
            "range": "± 403552",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 81280027,
            "range": "± 7796193",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 578327325,
            "range": "± 50076682",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 8283891,
            "range": "± 5206083",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 41121543,
            "range": "± 2330640",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 833043,
            "range": "± 37990",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25680214,
            "range": "± 408196",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 840663,
            "range": "± 45924",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27155557,
            "range": "± 1540150",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3679395,
            "range": "± 20213",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2837252,
            "range": "± 36175",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1068168,
            "range": "± 34076",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2164762,
            "range": "± 91542",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 763963,
            "range": "± 27598",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1451330,
            "range": "± 15232",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 724248,
            "range": "± 27945",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 705896,
            "range": "± 32801",
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
          "id": "f795bc44c866783a46d7d40d0142579a2d2f5566",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/f795bc44c866783a46d7d40d0142579a2d2f5566"
        },
        "date": 1769019727453,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1319141057,
            "range": "± 64919665",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2785210930,
            "range": "± 32943082",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193719053,
            "range": "± 8066075",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 309305804,
            "range": "± 3640403",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1819154647,
            "range": "± 191940253",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4973821,
            "range": "± 397999",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903624237,
            "range": "± 42544572",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3071481,
            "range": "± 225147",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24427867,
            "range": "± 2635239",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20152964,
            "range": "± 1799161",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22036419,
            "range": "± 2527497",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19481680,
            "range": "± 2455443",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15055822,
            "range": "± 1992970",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9408228,
            "range": "± 199001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 41083446,
            "range": "± 2106328",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8742272,
            "range": "± 253014",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79872027,
            "range": "± 7906426",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 561705092,
            "range": "± 43614756",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 9313406,
            "range": "± 5687887",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 41302946,
            "range": "± 3259108",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 828368,
            "range": "± 51387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25248332,
            "range": "± 675033",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 845321,
            "range": "± 21895",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26460564,
            "range": "± 592487",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3687521,
            "range": "± 44087",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2780846,
            "range": "± 26904",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 1082381,
            "range": "± 34397",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 2088399,
            "range": "± 52208",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 762689,
            "range": "± 19523",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1455005,
            "range": "± 18561",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 710151,
            "range": "± 12697",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 684777,
            "range": "± 38211",
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
          "id": "3012d99814904a80694e177f8ba2a4ab3b514ab9",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/3012d99814904a80694e177f8ba2a4ab3b514ab9"
        },
        "date": 1769022099422,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1307602269,
            "range": "± 35129133",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2845547790,
            "range": "± 74062860",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 189332411,
            "range": "± 8051878",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 315208181,
            "range": "± 10590393",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1596503279,
            "range": "± 239102110",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4900537,
            "range": "± 900358",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903370456,
            "range": "± 568313",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2952841,
            "range": "± 1722398",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 26258044,
            "range": "± 2526037",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20709481,
            "range": "± 2865406",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 22016429,
            "range": "± 1940708",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 20350145,
            "range": "± 3216178",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15525399,
            "range": "± 6069566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9841372,
            "range": "± 209152",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 56259982,
            "range": "± 11170910",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9073267,
            "range": "± 278207",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 83212879,
            "range": "± 10397732",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 547308899,
            "range": "± 23006260",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 7959849,
            "range": "± 4798394",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 40397006,
            "range": "± 2882084",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 860705,
            "range": "± 36356",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25554595,
            "range": "± 657467",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 591227,
            "range": "± 31007",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27311320,
            "range": "± 1375558",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3713818,
            "range": "± 13550",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2343974,
            "range": "± 15922",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 598712,
            "range": "± 17603",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1201465,
            "range": "± 21012",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 754783,
            "range": "± 27477",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1398467,
            "range": "± 19664",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 410501,
            "range": "± 9289",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 32067,
            "range": "± 2054",
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
          "id": "cb6f49be4107890d0e575c7050b96f8380501bae",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/cb6f49be4107890d0e575c7050b96f8380501bae"
        },
        "date": 1769033194427,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1291624677,
            "range": "± 36952201",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2795677453,
            "range": "± 26353810",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 187854657,
            "range": "± 1493928",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 312180560,
            "range": "± 7604454",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1368385414,
            "range": "± 218732802",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4773692,
            "range": "± 504932",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903245236,
            "range": "± 31383908",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3183903,
            "range": "± 434457",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24112367,
            "range": "± 2681242",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19477991,
            "range": "± 1447544",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21091427,
            "range": "± 2496436",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19373158,
            "range": "± 3049744",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14069675,
            "range": "± 3556688",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9427033,
            "range": "± 285679",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 50845141,
            "range": "± 9903832",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8747470,
            "range": "± 301902",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79046820,
            "range": "± 10745117",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 499428942,
            "range": "± 31328123",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5369582,
            "range": "± 636043",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48530396,
            "range": "± 3321882",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 490913,
            "range": "± 26165",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25531864,
            "range": "± 632823",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 303126,
            "range": "± 22697",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26632620,
            "range": "± 1272408",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3716784,
            "range": "± 17624",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2320831,
            "range": "± 131037",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 333882,
            "range": "± 10127",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1205244,
            "range": "± 12405",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 394286,
            "range": "± 15952",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1388363,
            "range": "± 29939",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 143292,
            "range": "± 10179",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 30497,
            "range": "± 2230",
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
          "id": "d3f7659145ba81a51b2f9e6f4fbf57386abdf6a0",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/d3f7659145ba81a51b2f9e6f4fbf57386abdf6a0"
        },
        "date": 1769033769338,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1315265412,
            "range": "± 34616618",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2917600441,
            "range": "± 24128109",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 200830399,
            "range": "± 2688523",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 323609441,
            "range": "± 3490477",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1370960308,
            "range": "± 217102992",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4904676,
            "range": "± 551526",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903250737,
            "range": "± 31664972",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3120987,
            "range": "± 194703",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24884520,
            "range": "± 2921479",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20152828,
            "range": "± 1481752",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21590406,
            "range": "± 2191422",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19528525,
            "range": "± 2432129",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16577994,
            "range": "± 3010570",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9586087,
            "range": "± 309496",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53998713,
            "range": "± 12295709",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8919485,
            "range": "± 432894",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78639917,
            "range": "± 5250896",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 536572642,
            "range": "± 32836425",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 6028161,
            "range": "± 727819",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49673313,
            "range": "± 2582357",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 495518,
            "range": "± 35596",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25749805,
            "range": "± 670379",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 317702,
            "range": "± 14178",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26753006,
            "range": "± 667667",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3788333,
            "range": "± 17626",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2357663,
            "range": "± 33545",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 345124,
            "range": "± 9497",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1178396,
            "range": "± 15216",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 402231,
            "range": "± 14457",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1400700,
            "range": "± 22323",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 146446,
            "range": "± 3388",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 29685,
            "range": "± 1767",
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
          "id": "435fcabc8260050412d1372325927e9d8e65e31f",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/435fcabc8260050412d1372325927e9d8e65e31f"
        },
        "date": 1769036563692,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1169956771,
            "range": "± 38864333",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2584341422,
            "range": "± 31790181",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 168223068,
            "range": "± 2055617",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 287689626,
            "range": "± 8245439",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1368665388,
            "range": "± 218241456",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5156387,
            "range": "± 571754",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 904546564,
            "range": "± 51279727",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2981122,
            "range": "± 428473",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 20806613,
            "range": "± 1219928",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 16463833,
            "range": "± 1005968",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 16442983,
            "range": "± 1539195",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 15665822,
            "range": "± 1476504",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 11201101,
            "range": "± 2771322",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 7594545,
            "range": "± 227949",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 45313209,
            "range": "± 11885388",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7246668,
            "range": "± 169792",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 58629966,
            "range": "± 4371591",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 451282793,
            "range": "± 25238752",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5373618,
            "range": "± 701796",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 47810757,
            "range": "± 2954689",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 580987,
            "range": "± 22210",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 13919329,
            "range": "± 373814",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 362052,
            "range": "± 14982",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 14697679,
            "range": "± 3357807",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 2700957,
            "range": "± 16893",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2479073,
            "range": "± 91520",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 346424,
            "range": "± 72376",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1041441,
            "range": "± 77012",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 509520,
            "range": "± 61056",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1764311,
            "range": "± 103607",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 279715,
            "range": "± 54488",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 36307,
            "range": "± 20969",
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
          "id": "45af6413c80f79380eab0bf7b84b620c2e27138f",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/45af6413c80f79380eab0bf7b84b620c2e27138f"
        },
        "date": 1769065832486,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1277476352,
            "range": "± 33328444",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2769892569,
            "range": "± 42666585",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 192771491,
            "range": "± 6460521",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 306028694,
            "range": "± 3819080",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1373105522,
            "range": "± 235208884",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5019652,
            "range": "± 1125749",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903106314,
            "range": "± 31435943",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2927481,
            "range": "± 181986",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24397179,
            "range": "± 2752053",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20322717,
            "range": "± 1288387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20891130,
            "range": "± 2144535",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19471887,
            "range": "± 2772626",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16672390,
            "range": "± 3365069",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9580124,
            "range": "± 246814",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 54874125,
            "range": "± 9900398",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8909658,
            "range": "± 234650",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 77783424,
            "range": "± 6686479",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 515980736,
            "range": "± 26344297",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5385721,
            "range": "± 551411",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49534501,
            "range": "± 3999201",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 496718,
            "range": "± 40318",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25421932,
            "range": "± 617261",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 308299,
            "range": "± 13943",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26719058,
            "range": "± 1280903",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3750739,
            "range": "± 28270",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2290343,
            "range": "± 18693",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 338719,
            "range": "± 6861",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1171226,
            "range": "± 19546",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 388312,
            "range": "± 9838",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1372306,
            "range": "± 15956",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 144990,
            "range": "± 8655",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 29927,
            "range": "± 1910",
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
          "id": "2c9e50b94d9fca3e90e1713c3544248d9ee62290",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/2c9e50b94d9fca3e90e1713c3544248d9ee62290"
        },
        "date": 1769066648705,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1294570837,
            "range": "± 33194432",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2815546598,
            "range": "± 36046833",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 188260678,
            "range": "± 11540041",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 307962285,
            "range": "± 2683614",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1602308795,
            "range": "± 235525615",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4873127,
            "range": "± 988644",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902936965,
            "range": "± 715156",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3302533,
            "range": "± 242500",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24075810,
            "range": "± 5159210",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19903968,
            "range": "± 2015947",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20932845,
            "range": "± 2971699",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19477748,
            "range": "± 2551257",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14471867,
            "range": "± 2498807",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9370410,
            "range": "± 287877",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 52039873,
            "range": "± 10435738",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8754886,
            "range": "± 319042",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79714796,
            "range": "± 7129976",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 523062862,
            "range": "± 31833724",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5481604,
            "range": "± 1002660",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 50763477,
            "range": "± 2121896",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 499479,
            "range": "± 27718",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25570251,
            "range": "± 578376",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 311512,
            "range": "± 18498",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26975631,
            "range": "± 1668987",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3731261,
            "range": "± 13293",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2293499,
            "range": "± 17994",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 339199,
            "range": "± 15391",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1177968,
            "range": "± 15086",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 390409,
            "range": "± 7347",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1372159,
            "range": "± 24394",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 146738,
            "range": "± 5532",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 27208,
            "range": "± 2318",
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
          "id": "44f3c153ece0b2fe03f8935ffe89861f003e8833",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/44f3c153ece0b2fe03f8935ffe89861f003e8833"
        },
        "date": 1769111886799,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1227443842,
            "range": "± 24512413",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2738146649,
            "range": "± 26951044",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 183786284,
            "range": "± 2153244",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 300162911,
            "range": "± 2989052",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1812453952,
            "range": "± 231928997",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4622106,
            "range": "± 498209",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902900143,
            "range": "± 41714772",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2557105,
            "range": "± 1605077",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24248891,
            "range": "± 1234964",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19473629,
            "range": "± 1640561",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20023811,
            "range": "± 1474093",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19104255,
            "range": "± 2548107",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 13486819,
            "range": "± 4816766",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9124075,
            "range": "± 367169",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53415636,
            "range": "± 9311423",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8749064,
            "range": "± 136277",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 76810629,
            "range": "± 5550698",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 518867947,
            "range": "± 40744656",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5246258,
            "range": "± 514844",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48810296,
            "range": "± 1919616",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 483084,
            "range": "± 19777",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25192143,
            "range": "± 648303",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 294442,
            "range": "± 12600",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26739078,
            "range": "± 1218097",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3743857,
            "range": "± 28599",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2428147,
            "range": "± 25542",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 344231,
            "range": "± 8883",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1266954,
            "range": "± 33352",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 425648,
            "range": "± 7682",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1471654,
            "range": "± 41937",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 150786,
            "range": "± 3392",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 27321,
            "range": "± 1859",
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
          "id": "b95c95bb3cfa35caa5e40ae7ac426a9459188a74",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/b95c95bb3cfa35caa5e40ae7ac426a9459188a74"
        },
        "date": 1769113554441,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1455127149,
            "range": "± 58473001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2932928262,
            "range": "± 54156732",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 200776656,
            "range": "± 9440075",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 317490786,
            "range": "± 4654469",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1373333404,
            "range": "± 187883651",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5325100,
            "range": "± 715904",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903374256,
            "range": "± 42107081",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3010669,
            "range": "± 894955",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24512603,
            "range": "± 1609447",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20324385,
            "range": "± 1738629",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21084076,
            "range": "± 1579355",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19350727,
            "range": "± 3962558",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 22195104,
            "range": "± 5340718",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9683189,
            "range": "± 102818",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 52705891,
            "range": "± 11148059",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8983162,
            "range": "± 196281",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 77992778,
            "range": "± 6443256",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 509419319,
            "range": "± 31404453",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5304087,
            "range": "± 1186973",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 51037095,
            "range": "± 3652528",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 500874,
            "range": "± 24732",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25217415,
            "range": "± 885939",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 300519,
            "range": "± 22127",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26562358,
            "range": "± 947242",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3715386,
            "range": "± 29108",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2308845,
            "range": "± 31609",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 353483,
            "range": "± 19431",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1320186,
            "range": "± 58260",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 384567,
            "range": "± 5610",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1365830,
            "range": "± 17065",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 154496,
            "range": "± 12047",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 29260,
            "range": "± 2759",
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
          "id": "2d4f8ca91176b546ca34d829efed44120c037215",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/2d4f8ca91176b546ca34d829efed44120c037215"
        },
        "date": 1769114215326,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1380936088,
            "range": "± 34208706",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2889467167,
            "range": "± 38165158",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 206517277,
            "range": "± 4401139",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 330733216,
            "range": "± 12813752",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1368901234,
            "range": "± 218395198",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5484277,
            "range": "± 1651468",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903168407,
            "range": "± 373202",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3033289,
            "range": "± 228949",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 25704232,
            "range": "± 3011342",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 22326413,
            "range": "± 1546528",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21893203,
            "range": "± 3001285",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 21283904,
            "range": "± 2710984",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 18400780,
            "range": "± 2919195",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 10356002,
            "range": "± 1097618",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 55534029,
            "range": "± 10963902",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8657232,
            "range": "± 453218",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 85025953,
            "range": "± 11217436",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 544635699,
            "range": "± 25125728",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5471695,
            "range": "± 686783",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49882365,
            "range": "± 4812160",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 516737,
            "range": "± 41485",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 27402587,
            "range": "± 962859",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 333722,
            "range": "± 22925",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 28744311,
            "range": "± 1422506",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3890995,
            "range": "± 82306",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2453330,
            "range": "± 88480",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 369054,
            "range": "± 15021",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1359068,
            "range": "± 61885",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 428049,
            "range": "± 50981",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1500767,
            "range": "± 121808",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 171061,
            "range": "± 30868",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 32069,
            "range": "± 4631",
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
          "id": "4d5be400a55bae4578f1e49a548f95c25598b91f",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/4d5be400a55bae4578f1e49a548f95c25598b91f"
        },
        "date": 1769114475191,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1276064213,
            "range": "± 33995945",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2730508433,
            "range": "± 43009182",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 184137669,
            "range": "± 4083352",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 304955879,
            "range": "± 3914499",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1370920522,
            "range": "± 216469285",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4766129,
            "range": "± 1019330",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902703926,
            "range": "± 624236",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3055961,
            "range": "± 231681",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23436721,
            "range": "± 1500996",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19358872,
            "range": "± 1605359",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 19911127,
            "range": "± 3658278",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19103513,
            "range": "± 2335080",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15555847,
            "range": "± 4030601",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9213647,
            "range": "± 161955",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 54112238,
            "range": "± 11905688",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8524426,
            "range": "± 278459",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 75935199,
            "range": "± 11255136",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 496546134,
            "range": "± 27067674",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5006146,
            "range": "± 615188",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 47797165,
            "range": "± 1492268",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 479697,
            "range": "± 22031",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 24956600,
            "range": "± 691387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 291517,
            "range": "± 14201",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26496233,
            "range": "± 1288125",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3668679,
            "range": "± 24686",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2321050,
            "range": "± 51979",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 338338,
            "range": "± 14490",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1212712,
            "range": "± 25267",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 383420,
            "range": "± 19138",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1355570,
            "range": "± 13156",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 149345,
            "range": "± 5428",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 27480,
            "range": "± 2296",
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
          "id": "2495915a4f601818b9a9d5468a999488cc36ddda",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/2495915a4f601818b9a9d5468a999488cc36ddda"
        },
        "date": 1769116903996,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1239383475,
            "range": "± 29801514",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2726663503,
            "range": "± 25264698",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 184394427,
            "range": "± 6965097",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 299280362,
            "range": "± 3060320",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1812339556,
            "range": "± 231983035",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4506705,
            "range": "± 775844",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 901950820,
            "range": "± 1056101",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2869679,
            "range": "± 701818",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24058776,
            "range": "± 2696695",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19023378,
            "range": "± 1549942",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 19957975,
            "range": "± 2038487",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19240347,
            "range": "± 3230464",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14830366,
            "range": "± 3331385",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9127890,
            "range": "± 287903",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 52065662,
            "range": "± 9513271",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8578416,
            "range": "± 365057",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 75987211,
            "range": "± 7873711",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 512042815,
            "range": "± 36989046",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5230688,
            "range": "± 532008",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48112283,
            "range": "± 2922583",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 475918,
            "range": "± 44160",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25009576,
            "range": "± 547828",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 291787,
            "range": "± 17102",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26187282,
            "range": "± 765201",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3724600,
            "range": "± 28828",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2373839,
            "range": "± 29481",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 334175,
            "range": "± 9853",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1209833,
            "range": "± 30577",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 421934,
            "range": "± 12818",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1454011,
            "range": "± 13051",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 143599,
            "range": "± 6535",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 26890,
            "range": "± 2057",
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
          "id": "41272bfc941f3f7b6ac7960683cf8da28f50eb42",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/41272bfc941f3f7b6ac7960683cf8da28f50eb42"
        },
        "date": 1769118630572,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1201580864,
            "range": "± 37533050",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2623942331,
            "range": "± 34826434",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 170298136,
            "range": "± 8129691",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 287534252,
            "range": "± 2798284",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1373397945,
            "range": "± 232041804",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5331163,
            "range": "± 496692",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 955514953,
            "range": "± 52160852",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3059876,
            "range": "± 257283",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 20958382,
            "range": "± 1942959",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 16346368,
            "range": "± 1638901",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 16523294,
            "range": "± 1078769",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 16043098,
            "range": "± 2368044",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 12009640,
            "range": "± 3204671",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 7656295,
            "range": "± 266943",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 47566266,
            "range": "± 13593566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7075151,
            "range": "± 168907",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 58256061,
            "range": "± 3446188",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 452008764,
            "range": "± 38337495",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5436062,
            "range": "± 573433",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 50281500,
            "range": "± 3725303",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 594822,
            "range": "± 15526",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 14006538,
            "range": "± 334032",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 379555,
            "range": "± 22899",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 14814693,
            "range": "± 909165",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 2767214,
            "range": "± 19914",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2475346,
            "range": "± 30946",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 322913,
            "range": "± 14200",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1014861,
            "range": "± 4106",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 474556,
            "range": "± 12931",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1587753,
            "range": "± 15647",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 180194,
            "range": "± 8017",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 33797,
            "range": "± 2424",
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
          "id": "abf66ef673883c39decd293f45538d9644ccd50a",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/abf66ef673883c39decd293f45538d9644ccd50a"
        },
        "date": 1769156825180,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1343675150,
            "range": "± 41835353",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2876476596,
            "range": "± 35108094",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 195058294,
            "range": "± 2170039",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 315320090,
            "range": "± 3749205",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1817430526,
            "range": "± 230292715",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5481395,
            "range": "± 620647",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903735419,
            "range": "± 42037559",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3592054,
            "range": "± 473098",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24672349,
            "range": "± 2867030",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20275666,
            "range": "± 1863700",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21316941,
            "range": "± 3207342",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19787318,
            "range": "± 3430785",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 19612572,
            "range": "± 4921677",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9764933,
            "range": "± 132737",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 56059668,
            "range": "± 11622405",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9003361,
            "range": "± 359238",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79330374,
            "range": "± 6232782",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 556421620,
            "range": "± 33645745",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5858825,
            "range": "± 762467",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 50167173,
            "range": "± 4242659",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 500227,
            "range": "± 96335",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25748802,
            "range": "± 613719",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 308502,
            "range": "± 21592",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26994264,
            "range": "± 1308700",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3784752,
            "range": "± 26705",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2399780,
            "range": "± 42185",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 369710,
            "range": "± 11080",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1241256,
            "range": "± 29655",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 403668,
            "range": "± 9438",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1406572,
            "range": "± 32559",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 163145,
            "range": "± 5553",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 32295,
            "range": "± 5391",
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
          "id": "9eacfdd4162a948e7baef25eb60a846354edbd68",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/9eacfdd4162a948e7baef25eb60a846354edbd68"
        },
        "date": 1769163456633,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1305860548,
            "range": "± 32210655",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2801483705,
            "range": "± 28432739",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 191591171,
            "range": "± 8343311",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 315544425,
            "range": "± 7258324",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1813501539,
            "range": "± 233199183",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4859242,
            "range": "± 577523",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902257093,
            "range": "± 1614742",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2766512,
            "range": "± 140551",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24478017,
            "range": "± 1402779",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19653427,
            "range": "± 1295916",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20542096,
            "range": "± 1556296",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19296690,
            "range": "± 3246039",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15300901,
            "range": "± 2815279",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9489511,
            "range": "± 314553",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 54418678,
            "range": "± 11496138",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8929788,
            "range": "± 264776",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 80297622,
            "range": "± 4571343",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 552017859,
            "range": "± 38383210",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5509939,
            "range": "± 473154",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 50932468,
            "range": "± 4394440",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 500443,
            "range": "± 19832",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 26325799,
            "range": "± 956571",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 308205,
            "range": "± 8260",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27299420,
            "range": "± 2304657",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3770827,
            "range": "± 20517",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2358019,
            "range": "± 28019",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 364018,
            "range": "± 8188",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1254570,
            "range": "± 14347",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 396366,
            "range": "± 17789",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1395749,
            "range": "± 31112",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 153561,
            "range": "± 4271",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 28901,
            "range": "± 1401",
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
          "id": "53f6e6a4a31ad87ae6c4f4aaa72e016fcf4b289a",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/53f6e6a4a31ad87ae6c4f4aaa72e016fcf4b289a"
        },
        "date": 1769166798397,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1293421215,
            "range": "± 43742390",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2767195947,
            "range": "± 29957548",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 187517478,
            "range": "± 4115771",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 305730861,
            "range": "± 12401998",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1813923712,
            "range": "± 234264583",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4565344,
            "range": "± 531187",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902788416,
            "range": "± 694966",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3153591,
            "range": "± 335295",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24355569,
            "range": "± 1435320",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20598307,
            "range": "± 2058101",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20439615,
            "range": "± 1794084",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19143974,
            "range": "± 3754100",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14671022,
            "range": "± 2825743",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9293401,
            "range": "± 382491",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53488509,
            "range": "± 10396808",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8890812,
            "range": "± 264336",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 77198407,
            "range": "± 6460398",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 505945319,
            "range": "± 30072775",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 4980325,
            "range": "± 568291",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48564401,
            "range": "± 3126586",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 494701,
            "range": "± 26600",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25293558,
            "range": "± 739489",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 301452,
            "range": "± 29589",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26823194,
            "range": "± 1048579",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3729605,
            "range": "± 19575",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2380659,
            "range": "± 32536",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 363464,
            "range": "± 11570",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1259864,
            "range": "± 27989",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 432545,
            "range": "± 13195",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1465078,
            "range": "± 42320",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 155652,
            "range": "± 4697",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 27600,
            "range": "± 765",
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
          "id": "d41dbdf6ca41e4dba00b32e034ea9d01dd919edd",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/d41dbdf6ca41e4dba00b32e034ea9d01dd919edd"
        },
        "date": 1769176683737,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1311888956,
            "range": "± 38905387",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2823826764,
            "range": "± 48684150",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 188936375,
            "range": "± 1520698",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 312246222,
            "range": "± 3046014",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1819485779,
            "range": "± 216506924",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5023575,
            "range": "± 870532",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903107084,
            "range": "± 31483240",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3078523,
            "range": "± 458753",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24156046,
            "range": "± 1060008",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19961760,
            "range": "± 2170388",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20713441,
            "range": "± 1673621",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19595758,
            "range": "± 3057655",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14617564,
            "range": "± 6024748",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9546147,
            "range": "± 239242",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 51704360,
            "range": "± 12229427",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9018212,
            "range": "± 278889",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 77532406,
            "range": "± 6234849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 521559622,
            "range": "± 54667595",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5441156,
            "range": "± 852574",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 50175893,
            "range": "± 3906696",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 529573,
            "range": "± 23414",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25187615,
            "range": "± 625455",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 323087,
            "range": "± 20581",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26466518,
            "range": "± 1357183",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3751308,
            "range": "± 31799",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2408457,
            "range": "± 38196",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 411137,
            "range": "± 20503",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1450050,
            "range": "± 45902",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 424712,
            "range": "± 16112",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1412785,
            "range": "± 26250",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 175063,
            "range": "± 13813",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 32448,
            "range": "± 1693",
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
          "id": "16a529d8a2dcd5dbb345484872bf4ee17b81861a",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/16a529d8a2dcd5dbb345484872bf4ee17b81861a"
        },
        "date": 1769551169576,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1324697945,
            "range": "± 40456859",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2808787798,
            "range": "± 33340568",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 195417025,
            "range": "± 4124483",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 314186385,
            "range": "± 3576792",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1373179503,
            "range": "± 231077057",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4868609,
            "range": "± 385612",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902827663,
            "range": "± 31484223",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2776509,
            "range": "± 259935",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24362039,
            "range": "± 1964389",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19927357,
            "range": "± 2767095",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20958512,
            "range": "± 1727583",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19485523,
            "range": "± 3087604",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16307622,
            "range": "± 3717874",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9326916,
            "range": "± 365489",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53356095,
            "range": "± 9717074",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8820906,
            "range": "± 338271",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 77052117,
            "range": "± 12723707",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 504042745,
            "range": "± 27595711",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5219387,
            "range": "± 595513",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48661519,
            "range": "± 2296189",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 504849,
            "range": "± 31753",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25213339,
            "range": "± 645509",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 308773,
            "range": "± 16683",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26564112,
            "range": "± 1003693",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3717630,
            "range": "± 41533",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2297856,
            "range": "± 29441",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 348254,
            "range": "± 11009",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1209924,
            "range": "± 22314",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 403909,
            "range": "± 15517",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1395447,
            "range": "± 11707",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 153480,
            "range": "± 9347",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 27674,
            "range": "± 1736",
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
          "id": "c737e8acd5e20010e19419f7d01b1f0637a00e4d",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/c737e8acd5e20010e19419f7d01b1f0637a00e4d"
        },
        "date": 1769773966770,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1191398551,
            "range": "± 29196091",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2603976213,
            "range": "± 26844354",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 169863187,
            "range": "± 8066960",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 287643035,
            "range": "± 2573011",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1597671445,
            "range": "± 239484036",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5200884,
            "range": "± 428460",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 1002394588,
            "range": "± 51863343",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3038610,
            "range": "± 346832",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 20982925,
            "range": "± 4183958",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 16485113,
            "range": "± 941441",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 16509236,
            "range": "± 1399226",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 16002185,
            "range": "± 2307413",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 12322500,
            "range": "± 4872724",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 7810513,
            "range": "± 155139",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 46274973,
            "range": "± 12791114",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7241869,
            "range": "± 310206",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 60141642,
            "range": "± 5526192",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 454351360,
            "range": "± 31575907",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5800028,
            "range": "± 841262",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48336386,
            "range": "± 3821914",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 725184,
            "range": "± 59276",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 14104866,
            "range": "± 549563",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 452239,
            "range": "± 57683",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 14500067,
            "range": "± 831511",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 2860758,
            "range": "± 60724",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2637889,
            "range": "± 48678",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 400496,
            "range": "± 76104",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1123617,
            "range": "± 64821",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 506688,
            "range": "± 41713",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1775781,
            "range": "± 50034",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 232443,
            "range": "± 31959",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 44079,
            "range": "± 4099",
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
          "id": "9ccf4ac2498e258edd8e7abfc01d16d8fc48c759",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/9ccf4ac2498e258edd8e7abfc01d16d8fc48c759"
        },
        "date": 1769783158262,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1352169278,
            "range": "± 38130466",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2820703209,
            "range": "± 35618499",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 192641154,
            "range": "± 16059603",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 312847256,
            "range": "± 5251952",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1372779943,
            "range": "± 233464017",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5004066,
            "range": "± 575061",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903326128,
            "range": "± 480428",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3150335,
            "range": "± 316000",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24853007,
            "range": "± 2688372",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20216423,
            "range": "± 1859328",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21227481,
            "range": "± 2829491",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19834665,
            "range": "± 3406655",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 17433920,
            "range": "± 4471903",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9692009,
            "range": "± 225924",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 54769057,
            "range": "± 10389684",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8925670,
            "range": "± 435134",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79348285,
            "range": "± 11744281",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 538954853,
            "range": "± 33872681",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5579754,
            "range": "± 828133",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 53663262,
            "range": "± 3218979",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 534725,
            "range": "± 45078",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 26096264,
            "range": "± 688655",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 322240,
            "range": "± 29355",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27486583,
            "range": "± 1263893",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3748475,
            "range": "± 29452",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2398395,
            "range": "± 17193",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 376995,
            "range": "± 19558",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1282893,
            "range": "± 47051",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 417168,
            "range": "± 7696",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1551361,
            "range": "± 24523",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 161021,
            "range": "± 7424",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 163539,
            "range": "± 6940",
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
          "id": "d7440eddac59881e4315261d85d1df22c2300b3c",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/d7440eddac59881e4315261d85d1df22c2300b3c"
        },
        "date": 1769789771688,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1140561665,
            "range": "± 33992560",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2589478127,
            "range": "± 26999333",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 169745023,
            "range": "± 10508384",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 284192313,
            "range": "± 8425464",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1372814627,
            "range": "± 233955956",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4920683,
            "range": "± 510410",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 904107091,
            "range": "± 48134044",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3119497,
            "range": "± 337920",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 20955268,
            "range": "± 3346138",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 16697469,
            "range": "± 1209711",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 16504959,
            "range": "± 1258731",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 16212802,
            "range": "± 2319444",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 13027075,
            "range": "± 3258960",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 7757679,
            "range": "± 268881",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 45730172,
            "range": "± 12412532",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7025946,
            "range": "± 352412",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 59683494,
            "range": "± 4808466",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 449094435,
            "range": "± 43202941",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5503400,
            "range": "± 1048228",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48254649,
            "range": "± 4760025",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 632408,
            "range": "± 59198",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 13955618,
            "range": "± 379314",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 398688,
            "range": "± 86859",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 14538485,
            "range": "± 747204",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 2786754,
            "range": "± 40366",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2581497,
            "range": "± 32219",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 333510,
            "range": "± 19015",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1135671,
            "range": "± 29627",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 504041,
            "range": "± 15239",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1990040,
            "range": "± 79329",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 184514,
            "range": "± 7275",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 192857,
            "range": "± 6807",
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
          "id": "abde9fc16d90ed45cb0b27c6be72a565fbe539b4",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/abde9fc16d90ed45cb0b27c6be72a565fbe539b4"
        },
        "date": 1769789843497,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1293996560,
            "range": "± 32917175",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2820281913,
            "range": "± 22794888",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193883232,
            "range": "± 2565890",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 315165466,
            "range": "± 7372755",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1365479880,
            "range": "± 188747352",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4511615,
            "range": "± 539838",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902949765,
            "range": "± 41861347",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2693466,
            "range": "± 213845",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23805143,
            "range": "± 1539513",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19287204,
            "range": "± 1980865",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20265266,
            "range": "± 1840695",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 18886876,
            "range": "± 3343513",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14194950,
            "range": "± 3428251",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9386938,
            "range": "± 300938",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53835485,
            "range": "± 9924068",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8791650,
            "range": "± 169318",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 76771612,
            "range": "± 5517560",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 516011688,
            "range": "± 41240276",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5183583,
            "range": "± 618746",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48309155,
            "range": "± 2553178",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 495370,
            "range": "± 46489",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25149419,
            "range": "± 531949",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 322294,
            "range": "± 15413",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26695805,
            "range": "± 1110447",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3687160,
            "range": "± 17834",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2345412,
            "range": "± 28013",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 345519,
            "range": "± 8748",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1216608,
            "range": "± 13976",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 401739,
            "range": "± 11249",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1509744,
            "range": "± 31866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 151917,
            "range": "± 5534",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 153935,
            "range": "± 6123",
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
          "id": "c064b503dffafaeefbe9b44dd75ccff0fd17c79c",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/c064b503dffafaeefbe9b44dd75ccff0fd17c79c"
        },
        "date": 1769790415143,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1258974331,
            "range": "± 44804264",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2785589170,
            "range": "± 29841514",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 192836874,
            "range": "± 2444764",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 315447802,
            "range": "± 5020214",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1379412663,
            "range": "± 230603165",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5511101,
            "range": "± 888884",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903089187,
            "range": "± 538871",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3547396,
            "range": "± 389254",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24170480,
            "range": "± 1738196",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20205388,
            "range": "± 1825428",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20985493,
            "range": "± 1804516",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19379082,
            "range": "± 3881087",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15113622,
            "range": "± 4791524",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9387421,
            "range": "± 381786",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53125461,
            "range": "± 12427996",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8813033,
            "range": "± 269866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79139735,
            "range": "± 5287780",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 534351286,
            "range": "± 29043784",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5426245,
            "range": "± 1194544",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49389247,
            "range": "± 2836737",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 504901,
            "range": "± 26860",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25303000,
            "range": "± 602071",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 324376,
            "range": "± 10697",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26678359,
            "range": "± 1234515",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3724644,
            "range": "± 14246",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2382556,
            "range": "± 48348",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 370750,
            "range": "± 10964",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1261419,
            "range": "± 45566",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 425834,
            "range": "± 10849",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1403023,
            "range": "± 9618",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 158150,
            "range": "± 6326",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 28369,
            "range": "± 1216",
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
          "id": "ad9e4f64439791727bedc597e46c652e2b17d46c",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/ad9e4f64439791727bedc597e46c652e2b17d46c"
        },
        "date": 1769791174387,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1283388081,
            "range": "± 34411291",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2803076909,
            "range": "± 74741580",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 186273954,
            "range": "± 9419417",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 305559056,
            "range": "± 11165952",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1381120462,
            "range": "± 229037509",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5211744,
            "range": "± 691309",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903095019,
            "range": "± 1625955",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3287882,
            "range": "± 468111",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23766821,
            "range": "± 1509542",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20215049,
            "range": "± 2867466",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20508033,
            "range": "± 2316102",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19286091,
            "range": "± 2840633",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14556906,
            "range": "± 4277046",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9411304,
            "range": "± 196917",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 51542489,
            "range": "± 11395680",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8979608,
            "range": "± 189164",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 77715988,
            "range": "± 11740432",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 513192769,
            "range": "± 44613388",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5497618,
            "range": "± 565833",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49149080,
            "range": "± 3840755",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 498569,
            "range": "± 41761",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25020910,
            "range": "± 454505",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 301104,
            "range": "± 14187",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26367878,
            "range": "± 3922299",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3718690,
            "range": "± 34034",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2353482,
            "range": "± 14191",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 368450,
            "range": "± 22220",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1330514,
            "range": "± 39597",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 432456,
            "range": "± 10246",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1383952,
            "range": "± 30956",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 164703,
            "range": "± 3903",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 29101,
            "range": "± 851",
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
          "id": "44244f4697cd6aa1be2d74fa74229b0826f58a6a",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/44244f4697cd6aa1be2d74fa74229b0826f58a6a"
        },
        "date": 1769794064993,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1302148228,
            "range": "± 36745174",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2816255110,
            "range": "± 37586404",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 193698956,
            "range": "± 4212680",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 310010466,
            "range": "± 1967671",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1371976342,
            "range": "± 232632649",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4991833,
            "range": "± 584792",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902878588,
            "range": "± 31586834",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3203938,
            "range": "± 420829",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24148315,
            "range": "± 1670167",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20043904,
            "range": "± 1605609",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20656724,
            "range": "± 1664506",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19486538,
            "range": "± 3201531",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 13954899,
            "range": "± 4690994",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9573927,
            "range": "± 344011",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53449183,
            "range": "± 9052167",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8717847,
            "range": "± 228892",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78585407,
            "range": "± 6285703",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 525008217,
            "range": "± 54913735",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5528503,
            "range": "± 892418",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49196197,
            "range": "± 3882246",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 499699,
            "range": "± 43710",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25094656,
            "range": "± 544343",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 308153,
            "range": "± 20056",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26201168,
            "range": "± 1116572",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3703268,
            "range": "± 15181",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2287024,
            "range": "± 21174",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 343348,
            "range": "± 6039",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1196306,
            "range": "± 18881",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 403417,
            "range": "± 21144",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1376591,
            "range": "± 18331",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 152412,
            "range": "± 12041",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 28219,
            "range": "± 1189",
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
          "id": "e6e684c9519ac3848ff503e7249a7b123385a33d",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/e6e684c9519ac3848ff503e7249a7b123385a33d"
        },
        "date": 1769794148311,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1253853803,
            "range": "± 48364124",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2769156696,
            "range": "± 24300676",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 177187414,
            "range": "± 10366534",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 309051634,
            "range": "± 2366452",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1817896676,
            "range": "± 232870135",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4949522,
            "range": "± 368313",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903765278,
            "range": "± 754588",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3023107,
            "range": "± 196575",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23149904,
            "range": "± 1568862",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 17753107,
            "range": "± 1397866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 18556648,
            "range": "± 2250481",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 17255328,
            "range": "± 2353594",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14767053,
            "range": "± 2289824",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 8398603,
            "range": "± 596763",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 52249079,
            "range": "± 10517672",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 7852808,
            "range": "± 329720",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 71232188,
            "range": "± 6063408",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 531583213,
            "range": "± 34013253",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5215528,
            "range": "± 541833",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 46703120,
            "range": "± 2098785",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 492548,
            "range": "± 16049",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 26147158,
            "range": "± 558466",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 309004,
            "range": "± 9791",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27234913,
            "range": "± 1248308",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 4044811,
            "range": "± 24061",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2210516,
            "range": "± 20283",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 293772,
            "range": "± 12044",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1034797,
            "range": "± 9428",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 387127,
            "range": "± 9433",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1316665,
            "range": "± 36646",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 156866,
            "range": "± 4119",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 34028,
            "range": "± 1254",
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
          "id": "15535c077a91b9d7abef22b0390905f3b6ec0d09",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/15535c077a91b9d7abef22b0390905f3b6ec0d09"
        },
        "date": 1769794264370,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1352193807,
            "range": "± 52579570",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2830220383,
            "range": "± 34407671",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 189005641,
            "range": "± 1014921",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 310975647,
            "range": "± 2677966",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1597702450,
            "range": "± 238465275",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5582369,
            "range": "± 870238",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903948512,
            "range": "± 41815356",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3209018,
            "range": "± 764186",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24054556,
            "range": "± 1466525",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20433757,
            "range": "± 1807921",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20590639,
            "range": "± 2182330",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19153053,
            "range": "± 3420012",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 17031588,
            "range": "± 3289058",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9574018,
            "range": "± 200984",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 52175484,
            "range": "± 12401893",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8837873,
            "range": "± 186955",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78746968,
            "range": "± 6148092",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 524239382,
            "range": "± 40915750",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5511052,
            "range": "± 586734",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49690450,
            "range": "± 2557992",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 529337,
            "range": "± 32461",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25245191,
            "range": "± 602135",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 321212,
            "range": "± 31008",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26559763,
            "range": "± 1109940",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3771119,
            "range": "± 38751",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2331112,
            "range": "± 25522",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 362760,
            "range": "± 15847",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1219811,
            "range": "± 29640",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 419180,
            "range": "± 13439",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1395449,
            "range": "± 22367",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 155217,
            "range": "± 7302",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 29409,
            "range": "± 1297",
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
          "id": "44cbbadc2f3f3a39457cea9ab6616105657303b4",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/44cbbadc2f3f3a39457cea9ab6616105657303b4"
        },
        "date": 1769796270713,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1314575639,
            "range": "± 35527917",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2820604937,
            "range": "± 29441309",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 189522063,
            "range": "± 10019473",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 311137876,
            "range": "± 7686324",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1368727536,
            "range": "± 142262876",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4906115,
            "range": "± 871419",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903009439,
            "range": "± 42045111",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2936484,
            "range": "± 463985",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 23906806,
            "range": "± 1574871",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19856342,
            "range": "± 1667733",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20622253,
            "range": "± 1842330",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19596005,
            "range": "± 3334056",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 15171700,
            "range": "± 3323044",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9511738,
            "range": "± 262079",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53973148,
            "range": "± 10528042",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8853232,
            "range": "± 430568",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 79136602,
            "range": "± 5624099",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 527261357,
            "range": "± 42024155",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5490171,
            "range": "± 849231",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48529318,
            "range": "± 2586126",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 511451,
            "range": "± 46533",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25288579,
            "range": "± 504211",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 319317,
            "range": "± 23435",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26646445,
            "range": "± 1525242",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3737198,
            "range": "± 30203",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2381087,
            "range": "± 23195",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 360624,
            "range": "± 9930",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1222251,
            "range": "± 35287",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 431562,
            "range": "± 24774",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1459179,
            "range": "± 28309",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 162149,
            "range": "± 6283",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 40686,
            "range": "± 3946",
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
          "id": "8b91268423d45da31f1ce3bf348e644493e314de",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/8b91268423d45da31f1ce3bf348e644493e314de"
        },
        "date": 1769796547744,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1390376693,
            "range": "± 42149032",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2955733030,
            "range": "± 53758577",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 200686195,
            "range": "± 5058608",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 322959600,
            "range": "± 8087512",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1373847067,
            "range": "± 189693148",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5146665,
            "range": "± 398262",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903252178,
            "range": "± 31541466",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3171251,
            "range": "± 583258",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24190051,
            "range": "± 3464447",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19588695,
            "range": "± 1627625",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20790243,
            "range": "± 1210866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19276965,
            "range": "± 3677424",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 13761176,
            "range": "± 2886919",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9817138,
            "range": "± 294257",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53911975,
            "range": "± 10180428",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8738723,
            "range": "± 287306",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78530370,
            "range": "± 5536750",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 531084552,
            "range": "± 26645627",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5217509,
            "range": "± 580012",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49153205,
            "range": "± 2840010",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 486100,
            "range": "± 35001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25863663,
            "range": "± 504024",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 292006,
            "range": "± 15579",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27505761,
            "range": "± 1328862",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3690477,
            "range": "± 21414",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2302250,
            "range": "± 24614",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 348607,
            "range": "± 13059",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1213635,
            "range": "± 13461",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 398816,
            "range": "± 20639",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1385979,
            "range": "± 8980",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 152459,
            "range": "± 5707",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 39908,
            "range": "± 1819",
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
          "id": "1b0227bea5c22302d093c3893c012044c4f1e530",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/1b0227bea5c22302d093c3893c012044c4f1e530"
        },
        "date": 1769804398325,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1303436923,
            "range": "± 41934980",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2783256823,
            "range": "± 33193225",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 189089574,
            "range": "± 2760439",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 308072932,
            "range": "± 10079858",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1817717913,
            "range": "± 231673388",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4893844,
            "range": "± 931654",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903583871,
            "range": "± 41802848",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2703843,
            "range": "± 149590",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24052127,
            "range": "± 1607414",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20138646,
            "range": "± 2711498",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20476377,
            "range": "± 2149108",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19115248,
            "range": "± 2565674",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 16167441,
            "range": "± 2697982",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9603093,
            "range": "± 215834",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 52556246,
            "range": "± 10143277",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8770933,
            "range": "± 375448",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78713000,
            "range": "± 10383657",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 508281367,
            "range": "± 34101080",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5263382,
            "range": "± 742392",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 49435037,
            "range": "± 2377181",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 501087,
            "range": "± 22409",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 25674059,
            "range": "± 597120",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 315553,
            "range": "± 13226",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26948904,
            "range": "± 1448731",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3689515,
            "range": "± 29027",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2352877,
            "range": "± 25537",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 354138,
            "range": "± 11760",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1206920,
            "range": "± 15212",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 392505,
            "range": "± 15730",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1446480,
            "range": "± 16341",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 150661,
            "range": "± 5002",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 39296,
            "range": "± 1999",
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
          "id": "dbb1efc164ecd745e6c43f2312d4b6eace221aba",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/dbb1efc164ecd745e6c43f2312d4b6eace221aba"
        },
        "date": 1769866069109,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1378812064,
            "range": "± 45169048",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2891760238,
            "range": "± 36410271",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 197604003,
            "range": "± 11708276",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 318563591,
            "range": "± 6425087",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1372614875,
            "range": "± 218572352",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 4530487,
            "range": "± 565001",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 903395365,
            "range": "± 42072156",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 3178177,
            "range": "± 468243",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24978311,
            "range": "± 3053589",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 20370951,
            "range": "± 1931025",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 21617492,
            "range": "± 2966472",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 20193960,
            "range": "± 3050918",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 18086766,
            "range": "± 3462338",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9567873,
            "range": "± 219027",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 53475225,
            "range": "± 10787974",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 9052344,
            "range": "± 212751",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 82571732,
            "range": "± 12979055",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 537167427,
            "range": "± 23340757",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5841590,
            "range": "± 464483",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 53149652,
            "range": "± 4087043",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 513711,
            "range": "± 30075",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 26031320,
            "range": "± 513704",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 324404,
            "range": "± 28473",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 27477066,
            "range": "± 975880",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3786468,
            "range": "± 38943",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2365312,
            "range": "± 20367",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 373383,
            "range": "± 16460",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1231403,
            "range": "± 30989",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 411006,
            "range": "± 4908",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1490675,
            "range": "± 21114",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 162744,
            "range": "± 8120",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 47319,
            "range": "± 3610",
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
          "id": "82e2132e67d803b8a9ace6a086991246a33b68cf",
          "message": "[MR] Version 0.2 - refactoring, easier endpoint and middleware plugins",
          "timestamp": "2026-01-17T20:03:06Z",
          "url": "https://github.com/marcomq/mq-bridge/pull/14/commits/82e2132e67d803b8a9ace6a086991246a33b68cf"
        },
        "date": 1769869173864,
        "tool": "cargo",
        "benches": [
          {
            "name": "performance/aws_single_write",
            "value": 1251752247,
            "range": "± 29868051",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_single_read",
            "value": 2727360328,
            "range": "± 24500703",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_write",
            "value": 185233771,
            "range": "± 1664449",
            "unit": "ns/iter"
          },
          {
            "name": "performance/aws_batch_read",
            "value": 303578535,
            "range": "± 9805786",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_write",
            "value": 1365215885,
            "range": "± 219304146",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_single_read",
            "value": 5226167,
            "range": "± 1615577",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_write",
            "value": 902981855,
            "range": "± 48572041",
            "unit": "ns/iter"
          },
          {
            "name": "performance/kafka_batch_read",
            "value": 2824203,
            "range": "± 137052",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_write",
            "value": 24150927,
            "range": "± 1456293",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_single_read",
            "value": 19853279,
            "range": "± 1473711",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_write",
            "value": 20442068,
            "range": "± 1570670",
            "unit": "ns/iter"
          },
          {
            "name": "performance/amqp_batch_read",
            "value": 19210045,
            "range": "± 2809028",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_write",
            "value": 14683950,
            "range": "± 3256763",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_single_read",
            "value": 9320017,
            "range": "± 233685",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_write",
            "value": 50882489,
            "range": "± 10399291",
            "unit": "ns/iter"
          },
          {
            "name": "performance/nats_batch_read",
            "value": 8825694,
            "range": "± 332636",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_write",
            "value": 78058255,
            "range": "± 12878880",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_single_read",
            "value": 500775440,
            "range": "± 18656693",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_write",
            "value": 5327499,
            "range": "± 496843",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mongodb_batch_read",
            "value": 48097842,
            "range": "± 2819681",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_write",
            "value": 488588,
            "range": "± 24889",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_single_read",
            "value": 24973834,
            "range": "± 559626",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_write",
            "value": 309408,
            "range": "± 17394",
            "unit": "ns/iter"
          },
          {
            "name": "performance/mqtt_batch_read",
            "value": 26575241,
            "range": "± 853196",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_write",
            "value": 3718539,
            "range": "± 30631",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_single_read",
            "value": 2372400,
            "range": "± 57921",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_write",
            "value": 352627,
            "range": "± 23866",
            "unit": "ns/iter"
          },
          {
            "name": "performance/zeromq_batch_read",
            "value": 1283787,
            "range": "± 30495",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_write",
            "value": 431327,
            "range": "± 19983",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_single_read",
            "value": 1493143,
            "range": "± 31904",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_write",
            "value": 157222,
            "range": "± 5971",
            "unit": "ns/iter"
          },
          {
            "name": "performance/memory_batch_read",
            "value": 39475,
            "range": "± 1337",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}