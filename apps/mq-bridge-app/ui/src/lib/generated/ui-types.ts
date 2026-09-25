/* This file is generated from Rust schemars schemas. Do not edit by hand. */



export interface AppConfig {
  log_level?: string;
  logger?: string;
  ui_addr?: string;
  metrics_addr?: string;
  plugins?: string[];
  routes?: Record<string, RouteConfig>;
  consumers?: ConsumerConfig[];
  publishers?: PublisherClient[];
  history?: Record<string, unknown>;
  env_vars?: Record<string, string>;
  config_security?: ConfigSecurity | null;
  extract_secrets?: boolean;
  default_tab?: string;
}

export interface ConsumerConfig {
  id?: string;
  name?: string;
  enabled?: boolean;
  endpoint: Endpoint;
  comment?: string;
  response?: ConsumerResponseConfig | null;
  output?: ConsumerOutputConfig;
  message_capture?: ConsumerMessageCaptureConfig;
  description?: string;
  concurrency?: number;
  batch_size?: number;
  commit_concurrency_limit?: number;
  startup_timeout_ms?: number;
  reconnect_interval_ms?: number;
  empty_batch_delay_ms?: number;
  allow_fault_injection?: boolean;
  exit_on_empty?: boolean;
  required_delivery?: DeliveryGuarantee | null;
}

export interface PublisherClient {
  id?: string;
  name?: string;
  endpoint: Endpoint;
  comment?: string;
  payload?: string;
  headers?: HeaderRow[];
  sort_order?: number | null;
}

export interface PublishRequest {
  name?: string;
  publisher_id?: string | null;
  payload: string;
  metadata?: Record<string, string>;
  endpoint?: Endpoint | null;
}

export interface RuntimeStatusResponse {
  active_consumers: string[];
  active_routes: string[];
  route_throughput: Record<string, number>;
  consumers: Record<string, ConsumerStatusSnapshot>;
}

export interface PeerStatusResponse {
  current_instance_id: string;
  instances: InstanceStatus[];
}

export interface ConsumerStatusResponse {
  running: boolean;
  status: EndpointStatusSnapshot;
  outcome?: RouteOutcomeSnapshot | null;
}

export interface StorageSecurityInfoResponse {
  target: string;
  encrypted: boolean;
  persistent: boolean;
  key_source: string;
  key_store_available: boolean;
  encrypted_config_available: boolean;
  persistent_messages_available: boolean;
  config_encrypted: boolean;
  messages_encrypted: boolean;
  messages_persistent: boolean;
  reason?: string | null;
  message_key_hex?: string | null;
  kid?: string | null;
}

export interface FeatureAvailabilityResponse {
  ibm_mq: boolean;
  kafka: boolean;
  nats: boolean;
  amqp: boolean;
  mqtt: boolean;
  http: boolean;
  grpc: boolean;
  zeromq: boolean;
  mongodb: boolean;
  aws: boolean;
  sled: boolean;
  redis_streams: boolean;
  object_store: boolean;
}

export interface RouteConfig {
  enabled?: boolean;
  input: Endpoint;
  output?: Endpoint | null;
  description?: string;
  concurrency?: number;
  batch_size?: number;
  commit_concurrency_limit?: number;
  startup_timeout_ms?: number;
  reconnect_interval_ms?: number;
  empty_batch_delay_ms?: number;
  allow_fault_injection?: boolean;
  exit_on_empty?: boolean;
  required_delivery?: DeliveryGuarantee | null;
}

export interface Endpoint {
  middlewares?: Middleware[];
}

export type Middleware = Record<string, never>;

export interface DeduplicationMiddleware {
  store?: string | null;
  sled_path?: string | null;
  ttl_seconds: number;
  key?: string | null;
  replay_response?: boolean;
}

export type MetricsMiddleware = Record<string, never>;

export interface DeadLetterQueueMiddleware {
  endpoint: Endpoint;
}

export interface RetryMiddleware {
  max_attempts?: number;
  initial_interval_ms?: number;
  max_interval_ms?: number;
  multiplier?: number;
}

export interface RandomPanicMiddleware {
  mode?: FaultMode;
  trigger_on_message?: number | null;
  enabled?: boolean;
}

export type FaultMode = "panic" | "disconnect" | "timeout" | "json_format_error" | "nack";

export interface DelayMiddleware {
  delay_ms: number;
}

export interface TimeoutMiddleware {
  timeout_ms: number;
}

export interface WeakJoinMiddleware {
  group_by: string;
  expected_count: number;
  timeout_ms: number;
  branch_by?: string | null;
  required?: string[];
  on_timeout?: WeakJoinTimeout;
}

export type WeakJoinTimeout = "fire" | "discard";

export interface LimiterMiddleware {
  messages_per_second: number;
}

export interface BufferMiddleware {
  max_messages: number;
  max_delay_ms: number;
}

export interface CookieJarMiddleware {
  shared_scope?: string | null;
  cookie_metadata_key?: string;
  set_cookie_metadata_key?: string;
  capture_metadata_keys?: string[];
  export_metadata_prefix?: string | null;
  inject_metadata?: Record<string, string>;
  max_cookies?: number;
}

export interface TransformMiddleware {
  mapping?: Record<string, MappingRule>;
  expression?: string | null;
  schema?: unknown;
  schema_file?: string | null;
  coerce?: boolean;
  apply_defaults?: boolean;
  coerce_empty_as_null?: boolean;
  on_error?: TransformErrorPolicy;
}

export type MappingRule = string | DetailedMappingRule;

export interface DetailedMappingRule {
  path: string;
  default?: unknown;
  required?: boolean;
}

export type TransformErrorPolicy = "reject" | "pass_through";

export interface EncryptionConfig {
  cipher?: CipherKind;
  key_id?: string;
  key: string;
  decrypt_keys?: Record<string, string>;
  authenticate_metadata?: string[];
}

export type CipherKind = "xchacha20poly1305" | "aes256gcm";

export interface CompressionMiddleware {
  algorithm?: Compression;
  max_decompressed_bytes?: number | null;
}

export type Compression = "none" | "gzip" | "lz4" | "zstd";

export interface PackMiddleware {
  format?: PackFormat;
  max_messages?: number;
  max_bytes?: number;
  drop_message_id?: boolean;
}

export type PackFormat = "mqb" | "benthos_binary";

export interface UnpackMiddleware {
  format?: PackFormat;
  max_messages?: number | null;
}

export interface AwsConfig {
  queue_url?: string | null;
  topic_arn?: string | null;
  region?: string | null;
  endpoint_url?: string | null;
  access_key?: string | null;
  secret_key?: string | null;
  session_token?: string | null;
  max_messages?: number | null;
  wait_time_seconds?: number | null;
  binary_payload_mode?: boolean;
}

export interface KafkaConfig {
  url: string;
  topic?: string | null;
  username?: string | null;
  password?: string | null;
  tls?: TlsConfig;
  group_id?: string | null;
  source_metadata?: boolean;
  delayed_ack?: boolean;
  producer_options?: unknown[][] | null;
  consumer_options?: unknown[][] | null;
  shared?: boolean | null;
  partitions?: number | null;
  partition_key?: string | null;
}

export interface TlsConfig {
  required?: boolean;
  ca_file?: string | null;
  cert_file?: string | null;
  key_file?: string | null;
  cert_password?: string | null;
  accept_invalid_certs?: boolean;
}

export interface NatsConfig {
  url: string;
  subject?: string | null;
  stream?: string | null;
  username?: string | null;
  password?: string | null;
  tls?: TlsConfig;
  token?: string | null;
  request_reply?: boolean;
  request_timeout_ms?: number | null;
  delayed_ack?: boolean;
  deduplicate?: boolean;
  no_jetstream?: boolean;
  subscriber_mode?: boolean;
  source_metadata?: boolean;
  stream_max_messages?: number | null;
  deliver_policy?: NatsDeliverPolicy | null;
  stream_max_bytes?: number | null;
  prefetch_count?: number | null;
  shared?: boolean | null;
}

export type NatsDeliverPolicy = "all" | "last" | "new" | "last_per_subject";

export interface FileConfig {
  path: string;
  name_by?: NameBy;
  idempotency?: boolean | null;
  delimiter?: string | null;
  format?: FileFormat;
  compression?: Compression;
  encryption?: EncryptionConfig | null;
  source_metadata?: boolean;
}

export type NameBy = "auto" | "source_position" | "write_time";

export type FileFormat = "normal" | "json" | "text" | "raw" | "csv" | "parquet";

export interface DirSpoolConfig {
  path: string;
  naming_pattern?: string;
  shard_depth?: number;
  shard_width?: number;
  payload_extension?: string;
  metadata_extension?: string;
  atomic?: boolean;
  fsync?: SpoolFsync;
  done_file?: string;
  emit_done?: SpoolDone;
  producer_file?: string;
  consumer_file?: string;
  drain_on_read?: boolean;
  stop_on_done?: boolean;
  poll_interval_ms?: number;
  source_metadata?: boolean;
  claim?: SpoolClaim;
}

export type SpoolFsync = "chunk" | "off";

export type SpoolDone = "never" | "success" | "end";

export type SpoolClaim = "exclusive" | "warn" | "off";

export interface ObjectStoreConfig {
  url: string;
  name_by?: NameBy;
  idempotency?: boolean | null;
  format?: FileFormat;
  delimiter?: string | null;
  checkpoint_store?: string | null;
  cursor_id?: string | null;
  polling_interval_ms?: number | null;
  max_object_bytes?: number | null;
  date_partition?: boolean | null;
  date_partition_style?: DatePartitionStyle;
  extension?: string | null;
  compression?: Compression;
  encryption?: EncryptionConfig | null;
}

export type DatePartitionStyle = "nested" | "hive";

export type StaticConfig = string | Record<string, never>;

export interface MemoryConfig {
  topic?: string;
  capacity?: number | null;
  request_reply?: boolean;
  request_timeout_ms?: number | null;
  subscribe_mode?: boolean;
  enable_nack?: boolean;
  url?: string;
}

export interface SledConfig {
  path: string;
  tree?: string | null;
  read_from_start?: boolean;
  delete_after_read?: boolean;
}

export interface AmqpConfig {
  url: string;
  queue?: string | null;
  subscribe_mode?: boolean;
  source_metadata?: boolean;
  username?: string | null;
  password?: string | null;
  tls?: TlsConfig;
  exchange?: string | null;
  prefetch_count?: number | null;
  no_persistence?: boolean;
  no_declare_queue?: boolean;
  delayed_ack?: boolean;
}

export interface MongoDbConfig {
  url: string;
  collection?: string | null;
  username?: string | null;
  password?: string | null;
  tls?: TlsConfig;
  database: string;
  polling_interval_ms?: number | null;
  reply_polling_ms?: number | null;
  request_reply?: boolean;
  consume?: MongoConsume | null;
  receive_query?: string | null;
  source_metadata?: boolean;
  change_stream?: boolean;
  checkpoint_store?: string | null;
  request_timeout_ms?: number | null;
  ttl_seconds?: number | null;
  capped_size_bytes?: number | null;
  format?: MongoDbFormat;
  id_field?: string | null;
  report_outcome?: boolean;
  cursor_id?: string | null;
  meta_collection?: string | null;
  shared?: boolean | null;
}

export type MongoConsume = "consumer" | "snapshot" | "capture_new" | "capture_all";

export type MongoDbFormat = "normal" | "json" | "text" | "raw";

export interface MqttConfig {
  url: string;
  topic?: string | null;
  username?: string | null;
  password?: string | null;
  tls?: TlsConfig;
  client_id?: string | null;
  queue_capacity?: number | null;
  max_inflight?: number | null;
  qos?: number | null;
  clean_session?: boolean;
  keep_alive_seconds?: number | null;
  protocol?: MqttProtocol;
  session_expiry_interval?: number | null;
  delayed_ack?: boolean;
}

export type MqttProtocol = "v5" | "v3";

export interface HttpConfig {
  url: string;
  path?: string | null;
  method?: string | null;
  tls?: TlsConfig;
  workers?: number | null;
  message_id_header?: string | null;
  request_timeout_ms?: number | null;
  internal_buffer_size?: number | null;
  fire_and_forget?: boolean;
  pass_through_status?: boolean;
  receive_streamable?: boolean;
  inline_response_fast_path?: boolean | null;
  server_protocol?: HttpServerProtocol;
  stream_response_to?: Endpoint | null;
  batch_concurrency?: number | null;
  tcp_keepalive_ms?: number | null;
  pool_idle_timeout_ms?: number | null;
  compression?: Compression;
  compression_enabled?: boolean | null;
  compression_threshold_bytes?: number | null;
  concurrency_limit?: number | null;
  basic_auth?: unknown[] | null;
  custom_headers?: Record<string, string>;
  shared?: boolean | null;
}

export type HttpServerProtocol = "auto" | "http1_only" | "http2_only";

export interface WebSocketConfig {
  url: string;
  path?: string | null;
  message_id_header?: string | null;
  routed_queue_capacity?: number | null;
  backlog?: number | null;
  execution_mode?: WebSocketExecutionMode;
}

export type WebSocketExecutionMode = "auto" | "direct_only" | "routed";

export interface IbmMqConfig {
  url: string;
  queue?: string | null;
  topic?: string | null;
  queue_manager: string;
  channel: string;
  username?: string | null;
  password?: string | null;
  tls?: IbmTlsConfig;
  max_message_size?: number;
  wait_timeout_ms?: number;
  internal_buffer_size?: number | null;
  disable_status_inq?: boolean;
}

export interface IbmTlsConfig {
  required?: boolean;
  cipher_spec?: string | null;
  cert_file?: string | null;
  cert_password?: string | null;
  accept_invalid_certs?: boolean;
  key_repository?: string | null;
  key_repository_password?: string | null;
}

export interface ZeroMqConfig {
  url: string;
  socket_type?: ZeroMqSocketType | null;
  topic?: string | null;
  bind?: boolean;
  internal_buffer_size?: number | null;
  format?: ZeroMqFormat;
  backend?: ZeroMqBackend;
  request_timeout_ms?: number | null;
}

export type ZeroMqSocketType = "push" | "pull" | "pub" | "sub" | "req" | "rep";

export type ZeroMqFormat = "json" | "raw" | "raw_framed";

export type ZeroMqBackend = "zmq" | "omq" | "try_omq";

export interface RedisStreamsConfig {
  url: string;
  stream?: string | null;
  group?: string | null;
  consumer_name?: string | null;
  subscriber_mode?: boolean;
  block_ms?: number | null;
  read_from_start?: boolean;
  redelivery_timeout_ms?: number | null;
  maxlen?: number | null;
  approx_trim?: boolean | null;
  username?: string | null;
  password?: string | null;
  internal_buffer_size?: number | null;
  reader_connections?: number | null;
}

export interface GrpcConfig {
  url: string;
  topic?: string | null;
  consumer_id?: string | null;
  timeout_ms?: number | null;
  connect_timeout_ms?: number | null;
  request_timeout_ms?: number | null;
  idle_stream_timeout_ms?: number | null;
  overall_timeout_ms?: number | null;
  tls?: TlsConfig;
  server_mode?: boolean;
  initial_stream_window_size?: number | null;
  initial_connection_window_size?: number | null;
  concurrency_limit_per_connection?: number | null;
  http2_keepalive_interval_ms?: number | null;
  http2_keepalive_timeout_ms?: number | null;
  max_decoding_message_size?: number | null;
  max_encoding_message_size?: number | null;
  descriptor_set_path?: string | null;
  descriptor_set_bytes?: number[] | null;
  reflection?: boolean;
  service_name?: string | null;
  method_name?: string | null;
  request?: unknown;
  server_streaming?: boolean;
  metadata?: Record<string, string>;
  binary_metadata?: Record<string, number[]>;
  bearer_token?: string | null;
  api_key?: string | null;
  api_key_name?: string | null;
  shared?: boolean | null;
}

export interface SqlxConfig {
  url: string;
  username?: string | null;
  password?: string | null;
  table: string;
  insert_query?: string | null;
  select_query?: string | null;
  delete_after_read?: boolean;
  cursor_column?: string | null;
  cursor_id?: string | null;
  checkpoint_store?: string | null;
  auto_create_table?: boolean;
  bulk_copy?: boolean;
  polling_interval_ms?: number | null;
  max_polling_interval_ms?: number | null;
  publication?: string | null;
  slot_name?: string | null;
  create_publication?: boolean;
  source_metadata?: boolean;
  tls?: TlsConfig;
  max_connections?: number | null;
  min_connections?: number | null;
  acquire_timeout_ms?: number | null;
  idle_timeout_ms?: number | null;
  max_lifetime_ms?: number | null;
  test_before_acquire?: boolean | null;
  shared?: boolean | null;
}

export interface ClickHouseConfig {
  url: string;
  username?: string | null;
  password?: string | null;
  database?: string | null;
  table: string;
  columns?: Record<string, string> | null;
  async_insert?: boolean;
  wait_for_async_insert?: boolean | null;
  cursor_column?: string | null;
  cursor_id?: string | null;
  checkpoint_store?: string | null;
  select_columns?: string | null;
  polling_interval_ms?: number | null;
  max_polling_interval_ms?: number | null;
  request_timeout_ms?: number | null;
  connect_timeout_ms?: number | null;
  tls?: TlsConfig;
  compression?: Compression;
}

export interface PostgresCdcConfig {
  url: string;
  publication: string;
  consume?: PostgresConsume | null;
  source_metadata?: boolean;
  slot_name?: string;
  create_slot?: boolean;
  create_publication?: boolean;
  publication_tables?: string[];
  temporary_slot?: boolean;
  cursor_id?: string | null;
  checkpoint_store?: string | null;
  status_interval_ms?: number;
  tls?: TlsConfig;
}

export type PostgresConsume = "capture_new" | "capture_all" | "snapshot";

export interface SequenceConfig {
  endpoints: Endpoint[];
  cursor_id?: string | null;
  checkpoint_store?: string | null;
}

export interface StreamBufferConfig {
  topic: string;
  correlation_id?: string | null;
  capacity?: number | null;
  idle_ttl_secs?: number | null;
}

export interface SwitchConfig {
  metadata_key?: string;
  cases?: Record<string, Endpoint>;
  when?: SwitchCase[];
  default?: Endpoint | null;
}

export interface SwitchCase {
  if: string;
  to: Endpoint;
}

export type ResponseConfig = Record<string, never>;

export interface RequestForwardConfig {
  to: Endpoint;
  forward_to: Endpoint;
}

export type DeliveryGuarantee = "at_most_once" | "at_least_once" | "effectively_once";

export interface ConsumerResponseConfig {
  headers?: Record<string, string>;
  payload?: string;
}

export type ConsumerOutputConfig = { mode: "none" } | { publisher: string; publisher_id?: string | null; mode: "publisher" } | { response?: ConsumerResponseConfig | null; mode: "response" };

export interface ConsumerMessageCaptureConfig {
  enabled?: boolean;
  keep_last?: number;
}

export interface HeaderRow {
  key?: string;
  value?: string;
  enabled?: boolean;
}

export interface ConfigSecurity {
  mode?: ConfigSecurityMode;
}

export type ConfigSecurityMode = "unencrypted" | "balanced" | "env_temporary_messages" | "temporary_messages" | "sensitive" | "durable";

export interface PulsarConfig {
  url: string;
  topic?: string;
  subscription?: string;
}

export interface ConsumerStatusSnapshot {
  running: boolean;
  status: EndpointStatusSnapshot;
  throughput: number;
  message_sequence: number;
  capture_enabled: boolean;
  capture_keep_last: number;
  outcome?: RouteOutcomeSnapshot | null;
}

export interface EndpointStatusSnapshot {
  healthy: boolean;
  target: string;
  pending?: number | null;
  capacity?: number | null;
  error?: string | null;
  details?: unknown;
}

export type RouteOutcomeSnapshot = "completed" | "stopped" | "failed";

export interface InstanceStatus {
  schema_version: number;
  instance_id: string;
  pid: number;
  kind: InstanceKind;
  application_version: string;
  started_at_ms: number;
  last_seen_at_ms: number;
  workspace_id: string;
  workspace_label: string;
  consumers?: StatusEntity[];
  publishers?: StatusEntity[];
  routes?: StatusRoute[];
}

export type InstanceKind = "cli" | "mcp" | "tauri" | "web-ui";

export interface StatusEntity {
  id: string;
  label: string;
  endpoint: string;
  summary: StatusSummary;
}

export interface StatusSummary {
  running: boolean;
  healthy: boolean;
  pending?: number | null;
  capacity?: number | null;
  error?: string | null;
  throughput: number;
  message_sequence: number;
}

export interface StatusRoute {
  id: string;
  label: string;
  input: StatusEntity;
  output: StatusEntity;
  summary: StatusSummary;
}

