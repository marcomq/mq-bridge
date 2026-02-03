use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::AwsConfig;
use crate::traits::{
    BatchCommitFunc, ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher,
    PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_sns::config::Credentials;
use aws_sdk_sns::Client as SnsClient;
use aws_sdk_sqs::Client as SqsClient;
use std::any::Any;
use tracing::{error, trace};

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

                let body = body_str.into_bytes();
                let mut canonical = CanonicalMessage::new(body, None);

                if let Some(attrs) = msg.message_attributes {
                    for (k, v) in attrs {
                        if let Some(s) = v.string_value {
                            canonical.metadata.insert(k, s);
                        }
                    }
                }

                messages.push(canonical);
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
        let body = String::from_utf8(message.payload.to_vec())
            .context("AWS payload must be valid UTF-8")?;

        let mut errors = Vec::new();

        if let (Some(client), Some(url)) = (&self.sqs_client, &self.queue_url) {
            let mut req = client.send_message().queue_url(url).message_body(&body);
            for (k, v) in &message.metadata {
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
            for (k, v) in &message.metadata {
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

            for chunk in messages.chunks(10) {
                let mut entries = Vec::with_capacity(chunk.len());
                let mut valid_indices = Vec::with_capacity(chunk.len());
                for (i, msg) in chunk.iter().enumerate() {
                    let body = match String::from_utf8(msg.payload.to_vec()) {
                        Ok(s) => s,
                        Err(e) => {
                            failed_messages
                                .push((msg.clone(), PublisherError::NonRetryable(anyhow!(e))));
                            continue;
                        }
                    };

                    let mut entry = aws_sdk_sqs::types::SendMessageBatchRequestEntry::builder()
                        .id(format!("{}", i))
                        .message_body(body);

                    for (k, v) in &msg.metadata {
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
                    valid_indices.push(i);
                }

                if entries.is_empty() {
                    continue;
                }

                let resp_result = client
                    .send_message_batch()
                    .queue_url(url)
                    .set_entries(Some(entries))
                    .send()
                    .await;

                match resp_result {
                    Ok(resp) => {
                        if !resp.failed.is_empty() {
                            for failure in resp.failed {
                                if let Ok(id_idx) = failure.id.parse::<usize>() {
                                    if let Some(msg) = chunk.get(id_idx) {
                                        let err = if failure.sender_fault {
                                            PublisherError::NonRetryable(anyhow!(failure
                                                .message
                                                .unwrap_or_default()))
                                        } else {
                                            PublisherError::Retryable(anyhow!(failure
                                                .message
                                                .unwrap_or_default()))
                                        };
                                        failed_messages.push((msg.clone(), err));
                                    } else {
                                        error!(id = %failure.id, index = id_idx, chunk_size = chunk.len(), "Invalid index parsed from SQS failure ID. Skipping failure.");
                                    }
                                } else {
                                    error!(
                                        id = %failure.id,
                                        "Failed to parse index from SQS failure ID. Skipping failure."
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        for i in valid_indices {
                            failed_messages.push((
                                chunk[i].clone(),
                                PublisherError::Retryable(anyhow!("Batch send failed: {}", e)),
                            ));
                        }
                    }
                }
            }

            if failed_messages.is_empty() {
                Ok(SentBatch::Ack)
            } else {
                Ok(SentBatch::Partial {
                    responses: None,
                    failed: failed_messages,
                })
            }
        } else {
            Ok(SentBatch::Ack)
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn load_aws_config(config: &AwsConfig) -> aws_config::SdkConfig {
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
