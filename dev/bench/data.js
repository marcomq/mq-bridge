window.BENCHMARK_DATA = {
  "lastUpdate": 1767908566429,
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
      }
    ]
  }
}