# hot_queue
Rust library to access different stream and message queues.

TODO: I will add the actual endpoint code later. No need to already create it now.
For now, I will just create the configuration structs, so I can parse yaml and json files
correctly.

It is possible to use following seperately:
- endpoint - have most simple access, can be amqp, mongodb etc... (will be implemted later)
- route, contains 2 endpoints

Between endpoint and route, there can be multiple middleware:
- retry
- deduplication (optional via feature)
- dead letter queue
- metrics

The message that is shared is a CanonicalMessage, which has a message_id, a byte array as body -
but it will be implemented later.

Configuration Schema Overview
The application uses a hierarchical configuration system. The structure is defined by Rust structs in src/model.rs and interpreted by the serde and config crates.

The key to understanding the endpoint configuration from, out is the combination of serde attributes you've used:

YAML Structure
Here is an example config.yml file that demonstrates the full structure.

yaml
kafka_to_nats: # The top-level keys are the route names
  # (Optional) Number of concurrent processing tasks for this route
  concurrency: 10

  # The input/source endpoint for the route
  input:
    # (Optional) A list of middlewares to apply to each endpoint before or after send
    middlewares:
      deduplication:
        sled_path: "/tmp/hot_queue/dedup_db"
        ttl_seconds: 3600
      metrics:
        # metrics doesn't have much configuration, its presence enables it.
    kafka:
        # --- Kafka-specific fields ---
        # `topic` is from `KafkaConsumerEndpoint`
        topic: "input-topic"
        # `brokers` and `group_id` are from the flattened `KafkaConfig`
        brokers: "localhost:9092"
        group_id: "my-consumer-group"
        # Other optional KafkaConfig fields like `username`, `password`, `tls`, etc.
        tls: 
          required: true
          ca_file: "/path_to_ca" # optional
          cert_file:  "/path_to_cert" # optional
          key_file:  "/path_to_key" # optional
          cert_password: "password"
          accept_invalid_certs: true


  # The output/sink endpoint for the route
  output:
    nats:
        # --- NATS-specific fields ---
        # `subject` is from `NatsPublisherEndpoint`
        subject: "output-subject"
        # `url` is from the flattened `NatsConfig`
        url: "nats://localhost:4222"
        # Other optional NatsConfig fields like `username`, `password`, `token`, etc.

Environment Variable Structure
The config crate maps environment variables to the YAML structure. It uses a prefix (in your case, HQ) and a separator (__).

Here is how the kafka_to_nats route from the YAML example above would be defined using environment variables:
sh
# Route settings
export HQ__KAFKA_TO_NATS__CONCURRENCY=10

# Input endpoint (kafka_to_nats.input)
export HQ__KAFKA_TO_NATS__INPUT__KAFKA__TOPIC="input-topic"
export HQ__KAFKA_TO_NATS__INPUT__KAFKA__BROKERS="localhost:9092"
export HQ__KAFKA_TO_NATS__INPUT__KAFKA__GROUP_ID="my-consumer-group"

# Output endpoint (kafka_to_nats.output)
export HQ__KAFKA_TO_NATS__OUTPUT__NATS__SUBJECT="output-subject"
export HQ__KAFKA_TO_NATS__OUTPUT__NATS__URL="nats://localhost:4222"

# Dead-Letter Queue endpoint for the input (kafka_to_nats.input.middlewares.dlq)
export HQ__KAFKA_TO_NATS__INPUT__MIDDLEWARES__DLQ__NATS__SUBJECT="dlq-subject"
export HQ__KAFKA_TO_NATS__INPUT__MIDDLEWARES__DLQ__NATS__URL="nats://localhost:4222"
