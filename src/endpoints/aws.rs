use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::AwsConfig;
use crate::traits::{
    BatchCommitFunc, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_sns::config::Credentials;
use aws_sdk_sns::Client as SnsClient;
use aws_sdk_sqs::Client as SqsClient;
use base64::{engine::general_purpose, Engine as _};
use std::any::Any;
use std::collections::HashMap;
use tracing::{error, trace};

const AWS_BINARY_ENCODED_METADATA_KEY: &str = "aws_binary_encoded";
/// SQS limits: attributes per message, entries per batch, bytes per batch request.
const SQS_MAX_ATTRIBUTES: usize = 10;
const SQS_BATCH_ENTRIES: usize = 10;
const SQS_BATCH_BYTES: usize = 256 * 1024;

/// One received SQS message. A body marked `aws_binary_encoded` is decoded again.
fn canonical_from_sqs(
    body: String,
    attributes: Option<HashMap<String, aws_sdk_sqs::types::MessageAttributeValue>>,
    sqs_message_id: Option<&str>,
) -> CanonicalMessage {
    let mut metadata = HashMap::new();
    for (key, value) in attributes.unwrap_or_default() {
        // A sender must not set the reserved `mqb.src.*` keys.
        if crate::canonical_message::is_source_metadata_key(&key) {
            continue;
        }
        if let Some(text) = value.string_value {
            metadata.insert(key, text);
        }
    }
    let encoded = metadata
        .get(AWS_BINARY_ENCODED_METADATA_KEY)
        .is_some_and(|flag| flag == "true");
    let payload = match encoded.then(|| general_purpose::STANDARD.decode(&body)) {
        Some(Ok(bytes)) => {
            metadata.remove(AWS_BINARY_ENCODED_METADATA_KEY);
            bytes
        }
        Some(Err(e)) => {
            tracing::warn!(error = %e, "SQS body is marked aws_binary_encoded but is not base64");
            body.into_bytes()
        }
        None => body.into_bytes(),
    };
    // The SQS `MessageId` stays the same when the message is delivered again.
    let message_id =
        sqs_message_id.and_then(|id| crate::canonical_message::message_id_from_str(id).ok());
    let mut canonical = CanonicalMessage::new(payload, message_id);
    canonical.metadata = metadata;
    canonical
}

/// Whether SQS and SNS accept `name` as a message attribute name.
fn valid_attribute_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..")
        && !lower.starts_with("aws.")
        && !lower.starts_with("amazon.")
}

/// Body and attributes of one outgoing message.
struct Outgoing {
    body: String,
    attributes: HashMap<String, String>,
}

impl Outgoing {
    /// Metadata the services cannot carry is left out; a message they would refuse is an error.
    fn new(message: &CanonicalMessage, binary: bool, to_sqs: bool) -> anyhow::Result<Self> {
        let mut attributes = HashMap::with_capacity(message.metadata.len());
        for (key, value) in &message.metadata {
            if crate::canonical_message::is_source_metadata_key(key) || value.is_empty() {
                continue;
            }
            if !valid_attribute_name(key) {
                warn_invalid_attribute_name(key);
                continue;
            }
            attributes.insert(key.clone(), value.clone());
        }
        let body = encode_aws_payload(&message.payload, &mut attributes, binary)?;
        if body.is_empty() {
            return Err(anyhow!("AWS cannot carry an empty message body"));
        }
        if to_sqs && attributes.len() > SQS_MAX_ATTRIBUTES {
            return Err(anyhow!(
                "SQS carries at most {SQS_MAX_ATTRIBUTES} message attributes, the message has {}",
                attributes.len()
            ));
        }
        Ok(Self { body, attributes })
    }

    fn bytes(&self) -> usize {
        let attributes: usize = self
            .attributes
            .iter()
            .map(|(key, value)| key.len() + value.len() + "String".len())
            .sum();
        self.body.len() + attributes
    }
}

fn warn_invalid_attribute_name(name: &str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            name,
            "Metadata key is not a valid SQS/SNS attribute name; such keys are left out"
        );
    });
}

/// Splits into batch requests SQS accepts: its entry count and its total size.
fn sqs_chunks<T>(sized: Vec<(usize, T)>) -> Vec<Vec<T>> {
    let mut chunks: Vec<Vec<T>> = Vec::new();
    let mut chunk_bytes = 0;
    for (bytes, item) in sized {
        let fits = chunks.last().is_some_and(|chunk| {
            chunk.len() < SQS_BATCH_ENTRIES && chunk_bytes + bytes <= SQS_BATCH_BYTES
        });
        if !fits {
            chunks.push(Vec::new());
            chunk_bytes = 0;
        }
        chunk_bytes += bytes;
        chunks.last_mut().expect("pushed above").push(item);
    }
    chunks
}

pub struct AwsConsumer {
    client: SqsClient,
    queue_url: String,
    max_messages: i32,
    wait_time_seconds: i32,
}

impl AwsConsumer {
    pub async fn new(config: &AwsConfig) -> anyhow::Result<Self> {
        let aws_config = load_aws_config(config).await;
        let client = SqsClient::new(&aws_config);
        let queue_url = config
            .queue_url
            .clone()
            .ok_or_else(|| anyhow!("queue_url is required for AWS consumer"))?;

        Ok(Self {
            client,
            queue_url,
            max_messages: config.max_messages.unwrap_or(10).clamp(1, 10),
            wait_time_seconds: config.wait_time_seconds.unwrap_or(20).clamp(0, 20),
        })
    }
}

#[async_trait]
impl MessageConsumer for AwsConsumer {
    // SQS acks by deleting each message by its receipt handle, so commits are
    // independent and safe to run concurrently and out of order.
    fn commit_requires_order(&self) -> bool {
        false
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut messages = Vec::with_capacity(max_messages);
        let mut receipt_handles = Vec::with_capacity(max_messages);
        let mut wait_time = self.wait_time_seconds;

        loop {
            let remaining = max_messages - messages.len();
            if remaining == 0 {
                break;
            }
            let max_to_fetch = (remaining as i32).min(self.max_messages);

            trace!(
                queue_url = %self.queue_url,
                max_to_fetch,
                current_count = messages.len(),
                "Receiving AWS messages"
            );

            let resp = self
                .client
                .receive_message()
                .queue_url(&self.queue_url)
                .max_number_of_messages(max_to_fetch)
                .wait_time_seconds(wait_time)
                .message_attribute_names("All")
                .send()
                .await
                .map_err(|e| ConsumerError::Connection(anyhow!(e)))?;

            let sqs_messages = resp.messages.unwrap_or_default();
            let count = sqs_messages.len();

            if count == 0 {
                break;
            }

            for msg in sqs_messages {
                let receipt_handle = msg.receipt_handle;
                let body_str = msg.body.unwrap_or_default();

                if receipt_handle.is_none() {
                    let preview = if body_str.len() > 50 {
                        let cut = body_str
                            .char_indices()
                            .nth(50)
                            .map(|(i, _)| i)
                            .unwrap_or(body_str.len());
                        format!("{}...", &body_str[..cut])
                    } else {
                        body_str.clone()
                    };
                    tracing::warn!(
                        message_id = ?msg.message_id,
                        len = body_str.len(),
                        preview = %preview,
                        "AWS SQS message missing receipt_handle. Processing payload but cannot acknowledge/delete."
                    );
                }

                messages.push(canonical_from_sqs(
                    body_str,
                    msg.message_attributes,
                    msg.message_id.as_deref(),
                ));
                receipt_handles.push(receipt_handle);
            }

            if count < max_to_fetch as usize {
                break;
            }
            // Don't wait for subsequent fetches to fill the batch
            wait_time = 0;
        }

        if messages.is_empty() {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        trace!(
            count = messages.len(),
            queue_url = %self.queue_url,
            message_ids = ?LazyMessageIds(&messages),
            "Received batch of AWS SQS messages"
        );

        let client = self.client.clone();
        let queue_url = self.queue_url.clone();

        let commit: BatchCommitFunc = Box::new(move |dispositions: Vec<MessageDisposition>| {
            let client = client.clone();
            let queue_url = queue_url.clone();
            let handles = receipt_handles.clone();
            Box::pin(async move {
                process_aws_batch(&client, &queue_url, &handles, &dispositions).await
            })
        });

        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        let mut pending = None;
        let mut details = serde_json::json!({});
        let mut healthy = true;
        let mut error = None;

        match self
            .client
            .get_queue_attributes()
            .queue_url(&self.queue_url)
            .attribute_names(aws_sdk_sqs::types::QueueAttributeName::ApproximateNumberOfMessages)
            .attribute_names(
                aws_sdk_sqs::types::QueueAttributeName::ApproximateNumberOfMessagesNotVisible,
            )
            .send()
            .await
        {
            Ok(resp) => {
                if let Some(attrs) = resp.attributes {
                    if let Some(count_str) = attrs
                        .get(&aws_sdk_sqs::types::QueueAttributeName::ApproximateNumberOfMessages)
                    {
                        if let Ok(val) = count_str.parse::<usize>() {
                            pending = Some(val);
                        }
                    }
                    let details_map: std::collections::HashMap<String, String> = attrs
                        .iter()
                        .map(|(k, v)| (k.as_str().to_string(), v.clone()))
                        .collect();
                    details = serde_json::json!(details_map);
                }
            }
            Err(e) => {
                healthy = false;
                error = Some(e.to_string());
            }
        }

        EndpointStatus {
            healthy,
            target: self.queue_url.clone(),
            pending,
            details,
            error,
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn process_aws_batch(
    client: &SqsClient,
    queue_url: &str,
    handles: &[Option<String>],
    dispositions: &[MessageDisposition],
) -> anyhow::Result<()> {
    if handles.len() != dispositions.len() {
        return Err(anyhow::anyhow!(
            "AWS batch commit received mismatched disposition count: expected {}, got {}",
            handles.len(),
            dispositions.len()
        ));
    }
    let (delete_entries, nack_entries) = prepare_aws_entries(handles, dispositions);

    process_aws_deletes(client, queue_url, delete_entries).await?;
    process_aws_nacks(client, queue_url, nack_entries).await?;
    Ok(())
}

fn prepare_aws_entries(
    handles: &[Option<String>],
    dispositions: &[MessageDisposition],
) -> (
    Vec<aws_sdk_sqs::types::DeleteMessageBatchRequestEntry>,
    Vec<aws_sdk_sqs::types::ChangeMessageVisibilityBatchRequestEntry>,
) {
    let mut delete_entries = Vec::new();
    let mut nack_entries = Vec::new();

    for (i, (handle_opt, disposition)) in handles.iter().zip(dispositions).enumerate() {
        if let Some(handle) = handle_opt {
            match disposition {
                MessageDisposition::Ack => { /* proceed to delete */ }
                MessageDisposition::Reply(_) => {
                    // A route hands over every sink response this way: not worth a warning each.
                    tracing::debug!("AWS consumer cannot reply; the reply is dropped");
                }
                MessageDisposition::Nack => { /* handle below */ }
            }

            match disposition {
                MessageDisposition::Ack | MessageDisposition::Reply(_) => {
                    delete_entries.push(
                        aws_sdk_sqs::types::DeleteMessageBatchRequestEntry::builder()
                            .id(format!("{}", i))
                            .receipt_handle(handle)
                            .build()
                            .unwrap(),
                    );
                }
                MessageDisposition::Nack => {
                    nack_entries.push(
                        aws_sdk_sqs::types::ChangeMessageVisibilityBatchRequestEntry::builder()
                            .id(format!("{}", i))
                            .receipt_handle(handle)
                            .visibility_timeout(0)
                            .build()
                            .unwrap(),
                    );
                }
            }
        }
    }
    (delete_entries, nack_entries)
}

async fn process_aws_deletes(
    client: &SqsClient,
    queue_url: &str,
    entries: Vec<aws_sdk_sqs::types::DeleteMessageBatchRequestEntry>,
) -> anyhow::Result<()> {
    for chunk in entries.chunks(10) {
        match client
            .delete_message_batch()
            .queue_url(queue_url)
            .set_entries(Some(chunk.to_vec()))
            .send()
            .await
        {
            Ok(resp) => {
                if !resp.failed.is_empty() {
                    let count = resp.failed.len();
                    error!(queue_url = %queue_url, failed_count = count, "Partial failure deleting SQS messages");
                    for failure in resp.failed {
                        error!(id = ?failure.id, code = ?failure.code, message = ?failure.message, sender_fault = failure.sender_fault, "SQS delete failure detail");
                    }
                    return Err(anyhow::anyhow!(
                        "SQS delete batch failed for {} messages",
                        count
                    ));
                }
            }
            Err(e) => {
                error!(queue_url = %queue_url, error = %e, "Failed to delete SQS message batch");
                return Err(anyhow::anyhow!("Failed to delete SQS message batch: {}", e));
            }
        }
    }
    Ok(())
}

async fn process_aws_nacks(
    client: &SqsClient,
    queue_url: &str,
    entries: Vec<aws_sdk_sqs::types::ChangeMessageVisibilityBatchRequestEntry>,
) -> anyhow::Result<()> {
    for chunk in entries.chunks(10) {
        if let Err(e) = client
            .change_message_visibility_batch()
            .queue_url(queue_url)
            .set_entries(Some(chunk.to_vec()))
            .send()
            .await
        {
            error!(queue_url = %queue_url, error = %e, "Failed to change visibility for Nacked SQS messages");
            return Err(anyhow::anyhow!(
                "Failed to change visibility for Nacked SQS messages: {}",
                e
            ));
        }
    }
    Ok(())
}

pub struct AwsPublisher {
    sqs_client: Option<SqsClient>,
    sns_client: Option<SnsClient>,
    queue_url: Option<String>,
    topic_arn: Option<String>,
    binary_payload_mode: bool,
}

fn encode_aws_payload(
    payload: &[u8],
    metadata: &mut HashMap<String, String>,
    is_base64_encoded: bool,
) -> anyhow::Result<String> {
    let body = if is_base64_encoded {
        metadata.insert(
            AWS_BINARY_ENCODED_METADATA_KEY.to_string(),
            "true".to_string(),
        );
        general_purpose::STANDARD.encode(payload)
    } else {
        String::from_utf8(payload.to_vec()).context("AWS payload must be valid UTF-8")?
    };
    Ok(body)
}

impl AwsPublisher {
    pub async fn new(config: &AwsConfig) -> anyhow::Result<Self> {
        let aws_config = load_aws_config(config).await;

        let (sqs_client, queue_url) = if let Some(url) = &config.queue_url {
            (Some(SqsClient::new(&aws_config)), Some(url.clone()))
        } else {
            (None, None)
        };

        let (sns_client, topic_arn) = if let Some(arn) = &config.topic_arn {
            (Some(SnsClient::new(&aws_config)), Some(arn.clone()))
        } else {
            (None, None)
        };

        if sqs_client.is_none() && sns_client.is_none() {
            return Err(anyhow!(
                "Either queue_url or topic_arn must be provided for AWS publisher"
            ));
        }

        Ok(Self {
            sqs_client,
            sns_client,
            queue_url,
            topic_arn,
            binary_payload_mode: config.binary_payload_mode,
        })
    }
}

#[async_trait]
impl MessagePublisher for AwsPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(
            message_id = %format!("{:032x}", message.message_id),
            queue_url = ?self.queue_url,
            topic_arn = ?self.topic_arn,
            payload_size = message.payload.len(),
            "Publishing AWS message"
        );

        let Outgoing {
            body,
            attributes: message_attrs,
        } = Outgoing::new(
            &message,
            self.binary_payload_mode,
            self.sqs_client.is_some(),
        )
        .map_err(PublisherError::NonRetryable)?;
        let mut errors = Vec::new();

        if let (Some(client), Some(url)) = (&self.sqs_client, &self.queue_url) {
            let mut req = client.send_message().queue_url(url).message_body(&body);
            for (k, v) in &message_attrs {
                req = req.message_attributes(
                    k,
                    aws_sdk_sqs::types::MessageAttributeValue::builder()
                        .data_type("String")
                        .string_value(v)
                        .build()
                        .unwrap(),
                );
            }
            if let Err(e) = req.send().await {
                errors.push(anyhow!(e).context("Failed to send to SQS"));
            }
        }

        if let (Some(client), Some(arn)) = (&self.sns_client, &self.topic_arn) {
            let mut req = client.publish().topic_arn(arn).message(&body);
            for (k, v) in &message_attrs {
                req = req.message_attributes(
                    k,
                    aws_sdk_sns::types::MessageAttributeValue::builder()
                        .data_type("String")
                        .string_value(v)
                        .build()
                        .unwrap(),
                );
            }
            if let Err(e) = req.send().await {
                errors.push(anyhow!(e).context("Failed to send to SNS"));
            }
        }

        if !errors.is_empty() {
            let msg = errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(PublisherError::Retryable(anyhow!(
                "AWS publish failed: {}",
                msg
            )));
        }

        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(
            count = messages.len(),
            queue_url = ?self.queue_url,
            topic_arn = ?self.topic_arn,
            message_ids = ?LazyMessageIds(&messages),
            "Publishing batch of AWS messages"
        );

        if self.sns_client.is_some() {
            return crate::traits::send_batch_helper(self, messages, |publisher, message| {
                Box::pin(publisher.send(message))
            })
            .await;
        }

        if let (Some(client), Some(url)) = (&self.sqs_client, &self.queue_url) {
            let mut failed_messages = Vec::new();
            let mut outgoing = Vec::with_capacity(messages.len());
            for (index, message) in messages.iter().enumerate() {
                match Outgoing::new(message, self.binary_payload_mode, true) {
                    Ok(out) => outgoing.push((out.bytes(), (index, out))),
                    Err(e) => {
                        failed_messages.push((message.clone(), PublisherError::NonRetryable(e)))
                    }
                }
            }

            for chunk in sqs_chunks(outgoing) {
                let indices: Vec<usize> = chunk.iter().map(|(index, _)| *index).collect();
                let mut entries = Vec::with_capacity(chunk.len());
                for (index, out) in chunk {
                    let mut entry = aws_sdk_sqs::types::SendMessageBatchRequestEntry::builder()
                        .id(index.to_string())
                        .message_body(out.body);
                    for (k, v) in out.attributes {
                        entry = entry.message_attributes(
                            k,
                            aws_sdk_sqs::types::MessageAttributeValue::builder()
                                .data_type("String")
                                .string_value(v)
                                .build()
                                .unwrap(),
                        );
                    }
                    entries.push(entry.build().unwrap());
                }

                let resp_result = client
                    .send_message_batch()
                    .queue_url(url)
                    .set_entries(Some(entries))
                    .send()
                    .await;

                match resp_result {
                    Ok(resp) => {
                        for failure in resp.failed {
                            let Some(msg) = failure
                                .id
                                .parse::<usize>()
                                .ok()
                                .and_then(|index| messages.get(index))
                            else {
                                error!(id = %failure.id, "SQS reported a failure for an unknown batch entry id. Skipping failure.");
                                continue;
                            };
                            let reason = anyhow!(failure.message.unwrap_or_default());
                            let err = if failure.sender_fault {
                                PublisherError::NonRetryable(reason)
                            } else {
                                PublisherError::Retryable(reason)
                            };
                            failed_messages.push((msg.clone(), err));
                        }
                    }
                    Err(e) => {
                        for index in indices {
                            failed_messages.push((
                                messages[index].clone(),
                                PublisherError::Retryable(anyhow!("Batch send failed: {}", e)),
                            ));
                        }
                    }
                }
            }

            Ok(SentBatch::from_failures(failed_messages))
        } else {
            Ok(SentBatch::Ack)
        }
    }

    async fn status(&self) -> EndpointStatus {
        let mut healthy = true;
        let mut error = None;
        let mut details = serde_json::json!({});

        if let (Some(client), Some(url)) = (&self.sqs_client, &self.queue_url) {
            match client.get_queue_attributes().queue_url(url).send().await {
                Ok(_) => { /* SQS is healthy */ }
                Err(e) => {
                    healthy = false;
                    error = Some(format!("SQS: {}", e));
                }
            }
        }

        if let (Some(client), Some(arn)) = (&self.sns_client, &self.topic_arn) {
            if healthy {
                // Don't overwrite the first error
                match client.get_topic_attributes().topic_arn(arn).send().await {
                    Ok(resp) => {
                        if let Some(attrs) = resp.attributes {
                            details["sns_attributes"] = serde_json::json!(attrs);
                        }
                    }
                    Err(e) => {
                        healthy = false;
                        error = Some(format!("SNS: {}", e));
                    }
                }
            }
        }

        let target = self.queue_url.clone().or_else(|| self.topic_arn.clone());
        EndpointStatus {
            healthy,
            error,
            target: target.unwrap_or_default(),
            details,
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(crate) async fn load_aws_config(config: &AwsConfig) -> aws_config::SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(region) = &config.region {
        loader = loader.region(aws_config::Region::new(region.clone()));
    }
    if let Some(url) = &config.endpoint_url {
        loader = loader.endpoint_url(url);
    }

    if let (Some(access_key), Some(secret_key)) = (&config.access_key, &config.secret_key) {
        let credentials = Credentials::new(
            access_key.clone(),
            secret_key.clone(),
            config.session_token.clone(),
            None,
            "static",
        );
        loader = loader.credentials_provider(credentials);
    }

    loader.load().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(value: &str) -> aws_sdk_sqs::types::MessageAttributeValue {
        aws_sdk_sqs::types::MessageAttributeValue::builder()
            .data_type("String")
            .string_value(value)
            .build()
            .unwrap()
    }

    #[test]
    fn a_binary_payload_comes_back_as_the_bytes_that_were_sent() {
        let sent = CanonicalMessage::new(vec![0, 159, 146, 150], None);
        let out = Outgoing::new(&sent, true, true).unwrap();
        let attributes = out
            .attributes
            .iter()
            .map(|(key, value)| (key.clone(), attribute(value)))
            .collect();

        let received = canonical_from_sqs(out.body, Some(attributes), None);
        assert_eq!(&received.payload[..], &[0, 159, 146, 150]);
        assert!(!received
            .metadata
            .contains_key(AWS_BINARY_ENCODED_METADATA_KEY));
    }

    #[test]
    fn a_received_attribute_cannot_set_source_metadata() {
        let attributes = HashMap::from([
            ("mqb.src.kafka_offset".to_string(), attribute("7")),
            ("kept".to_string(), attribute("yes")),
        ]);
        let received = canonical_from_sqs("body".to_string(), Some(attributes), None);
        assert!(!received.metadata.contains_key("mqb.src.kafka_offset"));
        assert_eq!(
            received.metadata.get("kept").map(String::as_str),
            Some("yes")
        );
    }

    #[test]
    fn a_redelivered_message_keeps_its_message_id() {
        let sqs_id = Some("5fea7756-0ea4-451a-a703-a558b933e274");
        let first = canonical_from_sqs("body".to_string(), None, sqs_id);
        let again = canonical_from_sqs("body".to_string(), None, sqs_id);
        assert_eq!(first.message_id, again.message_id);
        assert_eq!(
            first.message_id,
            0x5fea7756_0ea4_451a_a703_a558b933e274_u128
        );
    }

    #[test]
    fn metadata_sqs_cannot_carry_is_left_out() {
        let mut message = CanonicalMessage::new(b"body".to_vec(), None);
        for (key, value) in [("empty", ""), ("http:header", "x"), ("AWS.reserved", "x")] {
            message.metadata.insert(key.to_string(), value.to_string());
        }
        message
            .metadata
            .insert("trace.id".to_string(), "1".to_string());

        let out = Outgoing::new(&message, false, true).unwrap();
        assert_eq!(out.attributes.keys().collect::<Vec<_>>(), ["trace.id"]);
    }

    #[test]
    fn a_message_sqs_would_refuse_fails_before_the_request() {
        let empty = CanonicalMessage::new(Vec::new(), None);
        assert!(Outgoing::new(&empty, false, true).is_err());

        let mut crowded = CanonicalMessage::new(b"body".to_vec(), None);
        for i in 0..=SQS_MAX_ATTRIBUTES {
            crowded.metadata.insert(format!("key{i}"), "v".to_string());
        }
        assert!(Outgoing::new(&crowded, false, true).is_err());
        // SNS has no such limit.
        assert!(Outgoing::new(&crowded, false, false).is_ok());
    }

    #[test]
    fn a_batch_request_stays_within_the_size_and_count_sqs_accepts() {
        let large = SQS_BATCH_BYTES / 2 + 1;
        let sizes = |chunks: Vec<Vec<usize>>| chunks.iter().map(Vec::len).collect::<Vec<_>>();

        let chunks = sqs_chunks((0..3).map(|i| (large, i)).collect());
        assert_eq!(sizes(chunks), [1, 1, 1]);

        let chunks = sqs_chunks((0..25).map(|i| (10, i)).collect());
        assert_eq!(sizes(chunks), [10, 10, 5]);
    }
}
