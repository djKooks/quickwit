// Copyright (C) 2021 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use anyhow::{bail, Context};
use async_trait::async_trait;
use futures::StreamExt;
use itertools::Itertools;
use quickwit_actors::{ActorExitStatus, Mailbox};
use quickwit_metastore::checkpoint::{Checkpoint, CheckpointDelta, PartitionId, Position};
use rdkafka::config::{ClientConfig, RDKafkaLogLevel};
use rdkafka::consumer::stream_consumer::StreamConsumer;
use rdkafka::consumer::{Consumer, ConsumerContext, Rebalance};
use rdkafka::error::{KafkaError, KafkaResult};
use rdkafka::message::BorrowedMessage;
use rdkafka::topic_partition_list::TopicPartitionList;
use rdkafka::util::Timeout;
use rdkafka::{ClientContext, Message, Offset};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::models::RawDocBatch;
use crate::source::{IndexerMessage, Source, SourceContext, TypedSourceFactory};

/// Required parameters for instantiating a `KafkaSource`.
#[derive(Clone, Deserialize, Serialize)]
pub struct KafkaSourceParams {
    /// Comma-separated list of host and port pairs that provide the initial addresses of Kafka
    /// brokers that act as the starting point for a Kafka client to discover the full set of
    /// alive servers in the cluster.
    pub bootstrap_servers: String,
    /// Specifies the name of the consumer group a Kafka consumer belongs to.
    pub group_id: String,
    /// Name of the topic that the source consumes.
    pub topic: String,
    /// When set to `true`, the source will terminate after reading the last message of each
    /// assigned partition. Otherwise, it will keep waiting for new incoming messages.
    pub enable_partition_eof: Option<bool>,
}

/// Factory for instantiating a `KafkaSource`.
pub struct KafkaSourceFactory;

#[async_trait]
impl TypedSourceFactory for KafkaSourceFactory {
    type Source = KafkaSource;
    type Params = KafkaSourceParams;

    async fn typed_create_source(
        params: KafkaSourceParams,
        checkpoint: Checkpoint,
    ) -> anyhow::Result<Self::Source> {
        KafkaSource::try_new(params, checkpoint).await
    }
}

struct KafkaSourceContext;

impl ClientContext for KafkaSourceContext {}

impl ConsumerContext for KafkaSourceContext {
    fn pre_rebalance(&self, rebalance: &Rebalance) {
        info!("Pre rebalance {:?}", rebalance);
    }

    fn post_rebalance(&self, rebalance: &Rebalance) {
        info!("Post rebalance {:?}", rebalance);
    }

    fn commit_callback(&self, result: KafkaResult<()>, _offsets: &TopicPartitionList) {
        info!("Committing offsets: {:?}", result);
    }
}

type KafkaSourceConsumer = StreamConsumer<KafkaSourceContext>;

#[derive(Default)]
pub struct KafkaSourceState {
    /// Partitions IDs assigned to the source.
    pub assigned_partition_ids: HashMap<i32, PartitionId>,
    /// Offset for each partition of the last message received.
    pub current_positions: HashMap<i32, Position>,
    /// Number of active partitions, i.e., that have not reached EOF.
    pub num_active_partitions: usize,
    /// Number of bytes processed by the source.
    pub num_bytes_processed: u64,
    /// Number of messages processed by the source (including invalid messages).
    pub num_messages_processed: u64,
    // Number of invalid messages, i.e., that were empty or could not be parsed.
    pub num_invalid_messages: u64,
}

/// A `KafkaSource` consumes a topic and forwards its messages to an `Indexer`.
pub struct KafkaSource {
    bootstrap_servers: String,
    group_id: String,
    topic: String,
    consumer: KafkaSourceConsumer,
    state: KafkaSourceState,
}

impl fmt::Debug for KafkaSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KafkaSource {{ bootstrap_servers: {}, group_id: {}, topic:{} }}",
            self.bootstrap_servers, self.group_id, self.topic
        )
    }
}

impl KafkaSource {
    /// Instantiates a new `KafkaSource`.
    pub async fn try_new(
        params: KafkaSourceParams,
        checkpoint: Checkpoint,
    ) -> anyhow::Result<KafkaSource> {
        let consumer = create_consumer(
            params.bootstrap_servers.as_str(),
            params.group_id.as_str(),
            params.enable_partition_eof.unwrap_or(false),
        )?;
        let topic = params.topic.as_str();
        let partition_ids = fetch_partition_ids(&consumer, topic).await?;
        let assigned_partition_ids = partition_ids
            .iter()
            .map(|partition_id| {
                (
                    partition_id.clone(),
                    PartitionId::from(partition_id.clone()),
                )
            })
            .collect();
        let watermarks = fetch_watermarks(&consumer, topic, &partition_ids).await?;
        let kafka_checkpoint = kafka_checkpoint_from_checkpoint(&checkpoint)?;
        let assignment = compute_assignment(topic, &partition_ids, &kafka_checkpoint, &watermarks)?;

        debug!(
            bootstrap_servers = ?params.bootstrap_servers.as_str(),
            group_id = ?params.group_id.as_str(),
            topic = ?params.topic.as_str(),
            assignment = ?assignment,
            "Starting Kafka source."
        );

        consumer
            .assign(&assignment)
            .context("Failed to resume from checkpoint.")?;

        let state = KafkaSourceState {
            assigned_partition_ids,
            num_active_partitions: partition_ids.len(),
            ..Default::default()
        };
        Ok(KafkaSource {
            bootstrap_servers: params.bootstrap_servers,
            group_id: params.group_id,
            topic: params.topic,
            consumer,
            state,
        })
    }
}

#[async_trait]
impl Source for KafkaSource {
    async fn emit_batches(
        &mut self,
        batch_sink: &Mailbox<IndexerMessage>,
        ctx: &SourceContext,
    ) -> Result<(), ActorExitStatus> {
        let mut docs = Vec::new();
        let mut checkpoint_delta = CheckpointDelta::default();

        let deadline = tokio::time::sleep(quickwit_actors::HEARTBEAT / 2);
        let mut message_stream = Box::pin(self.consumer.stream().take_until(deadline));

        while let Some(message_res) = message_stream.next().await {
            let message = match message_res {
                Ok(message) => message,
                Err(KafkaError::PartitionEOF(partition_id)) => {
                    self.state.num_active_partitions -= 1;
                    info!(
                        topic = ?self.topic.as_str(),
                        partition_id = ?partition_id,
                        num_active_partitions = ?self.state.num_active_partitions,
                        "Reached end of partition."
                    );
                    continue;
                }
                // FIXME: This is assuming that Kafka errors are not recoverable, it may not be the
                // case.
                Err(err) => return Err(ActorExitStatus::from(anyhow::anyhow!(err))),
            };
            if let Some(doc) = parse_message_payload(&message) {
                docs.push(doc);
            } else {
                self.state.num_invalid_messages += 1;
            }
            self.state.num_bytes_processed += message.payload_len() as u64;
            self.state.num_messages_processed += 1;

            let partition_id = self
                .state
                .assigned_partition_ids
                .get(&message.partition())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Received unexpected message from partition `{}`. Assigned partitions: \
                         `{{{}}}`.",
                        message.partition(),
                        self.state.assigned_partition_ids.keys().join(", "),
                    )
                })?
                .clone();
            let current_position = Position::from(message.offset());
            let previous_position = self
                .state
                .current_positions
                .insert(message.partition(), current_position.clone())
                .unwrap_or_else(|| previous_position_for_offset(message.offset()));
            checkpoint_delta
                .record_partition_delta(partition_id, previous_position, current_position)
                .context("Failed to record partition delta.")?;
        }
        if !checkpoint_delta.is_empty() {
            let batch = RawDocBatch {
                docs,
                checkpoint_delta,
            };
            ctx.send_message(batch_sink, IndexerMessage::from(batch))
                .await?;
        }
        if self.state.num_active_partitions == 0 {
            info!(topic = &self.topic.as_str(), "Reached end of topic.");
            ctx.send_message(batch_sink, IndexerMessage::EndOfSource)
                .await?;
            return Err(ActorExitStatus::Success);
        }
        Ok(())
    }

    fn observable_state(&self) -> serde_json::Value {
        let assigned_partition_ids: Vec<&i32> =
            self.state.assigned_partition_ids.keys().sorted().collect();
        let current_positions: Vec<(&i32, i64)> = self
            .state
            .current_positions
            .iter()
            .filter_map(|(partition_id, position)| match position {
                Position::Offset(offset_str) => offset_str
                    .parse::<i64>()
                    .ok()
                    .map(|offset| (partition_id, offset)),
                Position::Beginning => None,
            })
            .sorted()
            .collect();
        json!({
            "group_id": self.group_id,
            "topic":  self.topic,
            "assigned_partition_ids": assigned_partition_ids,
            "current_positions": current_positions,
            "num_active_partitions": self.state.num_active_partitions,
            "num_bytes_processed": self.state.num_bytes_processed,
            "num_messages_processed": self.state.num_messages_processed,
            "num_invalid_messages": self.state.num_invalid_messages,
        })
    }
}

/// Returns the preceding `Position` for the offset.
fn previous_position_for_offset(offset: i64) -> Position {
    if offset == 0 {
        Position::Beginning
    } else {
        Position::from(offset - 1)
    }
}

/// Creates a new `KafkaSourceConsumer`.
fn create_consumer(
    bootstrap_servers: &str,
    group_id: &str,
    enable_partition_eof: bool,
) -> anyhow::Result<KafkaSourceConsumer> {
    let consumer: KafkaSourceConsumer = ClientConfig::new()
        .set("bootstrap.servers", bootstrap_servers)
        .set("group.id", group_id)
        .set("enable.partition.eof", enable_partition_eof.to_string())
        .set("session.timeout.ms", "6000")
        .set("enable.auto.commit", "false")
        //.set("statistics.interval.ms", "30000")
        //.set("auto.offset.reset", "smallest")
        .set_log_level(RDKafkaLogLevel::Info) // TODO: read log level from env variable.
        .create_with_context(KafkaSourceContext)
        .with_context(|| {
            format!(
                "Failed to create consumer with bootstrap servers `{}` and group ID `{}`.",
                bootstrap_servers, group_id,
            )
        })?;
    Ok(consumer)
}

/// Represents a checkpoint with the Kafka native types: `i32` for partition IDs and `i64` for
/// offsets.
fn kafka_checkpoint_from_checkpoint(checkpoint: &Checkpoint) -> anyhow::Result<HashMap<i32, i64>> {
    let mut kafka_checkpoint = HashMap::with_capacity(checkpoint.num_partitions());
    for (partition_id, position) in checkpoint.iter() {
        let partition_i32 = partition_id.0.parse::<i32>().with_context(|| {
            format!("Failed to parse partition ID `{}` to i32.", partition_id.0)
        })?;
        let offset_i64 = match position {
            Position::Beginning => continue,
            Position::Offset(offset_str) => offset_str
                .parse::<i64>()
                .with_context(|| format!("Failed to parse offset `{}` to i64.", offset_str))?,
        };
        kafka_checkpoint.insert(partition_i32, offset_i64);
    }
    Ok(kafka_checkpoint)
}

/// Retrieves the list of all partition IDs of a given topic.
async fn fetch_partition_ids(
    consumer: &KafkaSourceConsumer,
    topic: &str,
) -> anyhow::Result<Vec<i32>> {
    let cluster_metadata = async move {
        let timeout = Timeout::After(Duration::from_secs(5));
        consumer
            .fetch_metadata(Some(topic), timeout)
            .with_context(|| format!("Failed to fetch metadata for topic `{}`.", topic))
    }
    .await?;

    if cluster_metadata.topics().is_empty() {
        bail!("Topic `{}` does not exist.", topic);
    }
    let topic_metadata = &cluster_metadata.topics()[0];
    assert!(topic_metadata.name() == topic); // Belt and suspenders.

    if topic_metadata.partitions().is_empty() {
        bail!("Topic `{}` has no partitions.", topic);
    }
    let partition_ids = topic_metadata
        .partitions()
        .iter()
        .map(|partition| partition.id())
        .collect();
    Ok(partition_ids)
}

/// Fetches the low and high watermarks for the given topic and partition IDs.
///
/// The low watermark is the offset of the earliest message in the partition. If no messages have
/// been written to the topic, the low watermark offset is set to 0. The low watermark will also be
/// 0 if one message has been written to the partition (with offset 0).
///
/// The high watermark is the offset of the latest message in the partition available for
/// consumption + 1.
async fn fetch_watermarks(
    consumer: &KafkaSourceConsumer,
    topic: &str,
    partition_ids: &[i32],
) -> anyhow::Result<HashMap<i32, (i64, i64)>> {
    let timeout = Duration::from_secs(10);
    let tasks = partition_ids.iter().map(|&partition_id| async move {
        consumer
            .fetch_watermarks(topic, partition_id, timeout)
            .map(|watermarks| (partition_id, watermarks))
            .with_context(|| {
                format!(
                    "Failed to fetch watermarks for topic `{}` and partition `{}`.",
                    topic, partition_id
                )
            })
    });
    let watermarks = futures::future::try_join_all(tasks)
        .await?
        .into_iter()
        .collect();
    Ok(watermarks)
}

/// Given a checkpoint, computes the next offset from which to start reading messages for the
/// provided partition IDs. See `compute_next_offset` for further explanation.
fn compute_assignment(
    topic: &str,
    partition_ids: &[i32],
    checkpoint: &HashMap<i32, i64>,
    watermarks: &HashMap<i32, (i64, i64)>,
) -> anyhow::Result<TopicPartitionList> {
    let mut assignment = TopicPartitionList::with_capacity(partition_ids.len());
    for &partition_id in partition_ids {
        let next_offset = compute_next_offset(partition_id, checkpoint, watermarks)?;
        let _ = assignment.add_partition_offset(topic, partition_id, next_offset)?;
    }
    Ok(assignment)
}

/// Given a checkpoint, computes the next offset from which to start reading messages. In most
/// cases, it should be the offset of the last checkpointed record + 1. However, when that offset no
/// longer exists in the partition (data loss, retention, ...), the next offset is the low
/// watermark. If a partition ID is not covered by a checkpoint, the partition is read from the
/// beginning.
fn compute_next_offset(
    partition_id: i32,
    checkpoint: &HashMap<i32, i64>,
    watermarks: &HashMap<i32, (i64, i64)>,
) -> anyhow::Result<Offset> {
    let checkpoint_offset = match checkpoint.get(&partition_id) {
        Some(&checkpoint_offset) => checkpoint_offset,
        None => return Ok(Offset::Beginning),
    };
    let (low_watermark, high_watermark) = match watermarks.get(&partition_id) {
        Some(&watermarks) => watermarks,
        None => bail!("Missing watermarks for partition `{}`.", partition_id),
    };
    // We found a gap between the last checkpoint and the low watermark, so we resume from the low
    // watermark.
    if checkpoint_offset < low_watermark {
        return Ok(Offset::Offset(low_watermark));
    }
    // This is the happy path, we resume right after the last checkpointed offset.
    if checkpoint_offset < high_watermark {
        return Ok(Offset::Offset(checkpoint_offset + 1));
    }
    // Remember, the high watermark is the offset of the last message in the partition + 1.
    bail!(
        "Last checkpointed offset `{}` is greater or equal to high watermark `{}`.",
        checkpoint_offset,
        high_watermark
    );
}

/// Converts the raw bytes of the message payload to a `String` skipping corrupted or empty
/// messages.
fn parse_message_payload(message: &BorrowedMessage) -> Option<String> {
    match message.payload_view::<str>() {
        Some(Ok(payload)) if payload.len() > 0 => {
            let doc = payload.to_string();
            debug!(
                topic = ?message.topic(),
                partition_id = ?message.partition(),
                offset = ?message.offset(),
                timestamp = ?message.timestamp(),
                num_bytes = ?message.payload_len(),
                doc = ?doc.as_str(),
                "Message received.",
            );
            return Some(doc);
        }
        Some(Ok(_)) => debug!(
            topic = ?message.topic(),
            partition = ?message.partition(),
            offset = ?message.offset(),
            timestamp = ?message.timestamp(),
            "Document is empty."
        ),
        Some(Err(error)) => warn!(
            topic = ?message.topic(),
            partition = ?message.partition(),
            offset = ?message.offset(),
            timestamp = ?message.timestamp(),
            error = ?error,
            "Failed to deserialize message payload."
        ),
        None => debug!(
            topic = ?message.topic(),
            partition = ?message.partition(),
            offset = ?message.offset(),
            timestamp = ?message.timestamp(),
            "Message payload is empty."
        ),
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_assignment() -> anyhow::Result<()> {
        let partition_ids = &[0, 1, 2];
        let checkpoint = vec![(1, 99), (2, 1337)].into_iter().collect();
        let watermarks = vec![(1, (50, 100)), (2, (1789, 2048))]
            .into_iter()
            .collect();
        let assignment = compute_assignment("topic", partition_ids, &checkpoint, &watermarks)?;
        let partitions = assignment.elements();
        assert_eq!(partitions.len(), 3);
        assert!(partitions
            .iter()
            .all(|partition| partition.topic() == "topic"));
        assert!(partitions
            .iter()
            .enumerate()
            .all(|(idx, partition)| partition.partition() == idx as i32));
        assert_eq!(partitions[0].offset(), Offset::Beginning);
        assert_eq!(partitions[1].offset(), Offset::Offset(100));
        assert_eq!(partitions[2].offset(), Offset::Offset(1789));
        Ok(())
    }

    #[test]
    fn test_compute_next_offset() -> anyhow::Result<()> {
        {
            let checkpoint = HashMap::new();
            let watermarks = HashMap::new();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks)?;
            assert_eq!(next_offset, Offset::Beginning);
        }
        {
            let checkpoint = vec![(0, 0)].into_iter().collect();
            let watermarks = vec![(0, (5, 10))].into_iter().collect();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks)?;
            assert_eq!(next_offset, Offset::Offset(5));
        }
        {
            let checkpoint = vec![(0, 4)].into_iter().collect();
            let watermarks = vec![(0, (5, 10))].into_iter().collect();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks)?;
            assert_eq!(next_offset, Offset::Offset(5));
        }
        {
            let checkpoint = vec![(0, 5)].into_iter().collect();
            let watermarks = vec![(0, (5, 10))].into_iter().collect();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks)?;
            assert_eq!(next_offset, Offset::Offset(6));
        }
        {
            let checkpoint = vec![(0, 7)].into_iter().collect();
            let watermarks = vec![(0, (5, 10))].into_iter().collect();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks)?;
            assert_eq!(next_offset, Offset::Offset(8));
        }
        {
            let checkpoint = vec![(0, 9)].into_iter().collect();
            let watermarks = vec![(0, (5, 10))].into_iter().collect();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks)?;
            assert_eq!(next_offset, Offset::Offset(10));
        }
        {
            let checkpoint = vec![(0, 0)].into_iter().collect();
            let watermarks = HashMap::new();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks);
            assert!(next_offset.is_err());
        }
        {
            let checkpoint = vec![(0, 10)].into_iter().collect();
            let watermarks = vec![(0, (5, 10))].into_iter().collect();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks);
            assert!(next_offset.is_err());
        }
        {
            let checkpoint = vec![(0, 11)].into_iter().collect();
            let watermarks = vec![(0, (5, 10))].into_iter().collect();
            let next_offset = compute_next_offset(0, &checkpoint, &watermarks);
            assert!(next_offset.is_err());
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "kafka-broker-external-service"))]
mod kafka_broker_tests {
    use quickwit_actors::{create_test_mailbox, Universe};
    use rand::distributions::Alphanumeric;
    use rand::Rng;
    use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
    use rdkafka::client::DefaultClientContext;
    use rdkafka::message::ToBytes;
    use rdkafka::producer::{FutureProducer, FutureRecord};

    use super::*;
    use crate::source::SourceActor;

    fn append_random_suffix(string: &str) -> String {
        let rng = rand::thread_rng();
        let slug: String = rng
            .sample_iter(&Alphanumeric)
            .take(4)
            .map(char::from)
            .collect();
        format!("{}-{}", string, slug)
    }

    fn create_admin_client(
        bootstrap_servers: &str,
    ) -> anyhow::Result<AdminClient<DefaultClientContext>> {
        let admin_client = ClientConfig::new()
            .set("bootstrap.servers", bootstrap_servers)
            .create()?;
        Ok(admin_client)
    }

    async fn create_topic(
        admin_client: &AdminClient<DefaultClientContext>,
        topic: &str,
        num_partitions: i32,
    ) -> anyhow::Result<()> {
        admin_client
            .create_topics(
                &[NewTopic::new(
                    topic,
                    num_partitions,
                    TopicReplication::Fixed(1),
                )],
                &AdminOptions::new().operation_timeout(Some(Duration::from_secs(5))),
            )
            .await?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|(topic, err_code)| {
                anyhow::anyhow!(
                    "Failed to create topic `{}`. Error code: `{}`",
                    topic,
                    err_code
                )
            })?;
        Ok(())
    }

    async fn populate_topic<K, M, J, Q>(
        bootstrap_servers: &str,
        topic: &str,
        num_messages: i32,
        key_fn: &K,
        message_fn: &M,
        partition: Option<i32>,
        timestamp: Option<i64>,
    ) -> anyhow::Result<HashMap<(i32, i64), i32>>
    where
        K: Fn(i32) -> J,
        M: Fn(i32) -> Q,
        J: ToBytes,
        Q: ToBytes,
    {
        let producer: &FutureProducer = &ClientConfig::new()
            .set("bootstrap.servers", bootstrap_servers)
            .set("statistics.interval.ms", "500")
            .set("api.version.request", "true")
            .set("debug", "all")
            .set("message.timeout.ms", "30000")
            .create()?;
        let tasks = (0..num_messages).map(|id| async move {
            producer
                .send(
                    FutureRecord {
                        topic,
                        partition,
                        timestamp,
                        key: Some(&key_fn(id)),
                        payload: Some(&message_fn(id)),
                        headers: None,
                    },
                    Duration::from_secs(1),
                )
                .await
                .map(|(partition, offset)| (id, partition, offset))
                .map_err(|(err, _)| err)
        });
        let message_map = futures::future::try_join_all(tasks)
            .await?
            .into_iter()
            .fold(HashMap::new(), |mut acc, (id, partition, offset)| {
                acc.insert((partition, offset), id);
                acc
            });
        Ok(message_map)
    }

    fn key_fn(id: i32) -> String {
        format!("Key {}", id)
    }

    fn message_fn(id: i32) -> String {
        format!("Message #{}", id)
    }

    #[tokio::test]
    async fn test_kafka_source() -> anyhow::Result<()> {
        quickwit_common::setup_logging_for_tests();

        let universe = Universe::new();

        let bootstrap_servers = "localhost:9092".to_string();
        let group_id = append_random_suffix("test-kafka-source-consumer-group");
        let topic = append_random_suffix("test-kafka-source-topic");

        let admin_client = create_admin_client(&bootstrap_servers)?;
        create_topic(&admin_client, &topic, 3).await?;

        let params = KafkaSourceParams {
            bootstrap_servers: bootstrap_servers.clone(),
            group_id: group_id.clone(),
            topic: topic.clone(),
            enable_partition_eof: Some(true),
        };
        {
            let (sink, inbox) = create_test_mailbox();
            let checkpoint = Checkpoint::default();
            let source = KafkaSource::try_new(params.clone(), checkpoint).await?;
            let actor = SourceActor {
                source: Box::new(source),
                batch_sink: sink.clone(),
            };
            let (_mailbox, handle) = universe.spawn_actor(actor).spawn_async();
            let (exit_status, exit_state) = handle.join().await;
            assert!(exit_status.is_success());

            let messages = inbox.drain_available_message_for_test();
            assert_eq!(messages.len(), 1);
            assert!(matches!(messages[0], IndexerMessage::EndOfSource));

            let expected_current_positions: Vec<(i32, i64)> = vec![];
            let expected_state = json!({
                "group_id": group_id,
                "topic":  topic,
                "assigned_partition_ids": vec![0, 1, 2],
                "current_positions":  expected_current_positions,
                "num_active_partitions": 0,
                "num_bytes_processed": 0,
                "num_messages_processed": 0,
                "num_invalid_messages": 0,
            });
            assert_eq!(exit_state, expected_state);
        }
        for partition_id in 0..3 {
            populate_topic(
                &bootstrap_servers,
                &topic,
                3,
                &key_fn,
                &|message_id| {
                    if message_id == 1 {
                        "".to_string()
                    } else {
                        format!("Message #{:0>3}", partition_id * 100 + message_id)
                    }
                },
                Some(partition_id),
                None,
            )
            .await?;
        }
        {
            let (sink, inbox) = create_test_mailbox();
            let checkpoint = Checkpoint::default();
            let source = KafkaSource::try_new(params.clone(), checkpoint).await?;
            let actor = SourceActor {
                source: Box::new(source),
                batch_sink: sink.clone(),
            };
            let (_mailbox, handle) = universe.spawn_actor(actor).spawn_async();
            let (exit_status, state) = handle.join().await;
            assert!(exit_status.is_success());

            let mut messages = inbox.drain_available_message_for_test();
            assert_eq!(messages.len(), 2);
            assert!(matches!(messages[0], IndexerMessage::Batch(_)));
            assert!(matches!(messages[1], IndexerMessage::EndOfSource));

            if let IndexerMessage::Batch(batch) = &mut messages[0] {
                batch.docs.sort();
                let expected_docs = vec![
                    "Message #000",
                    "Message #002",
                    "Message #100",
                    "Message #102",
                    "Message #200",
                    "Message #202",
                ];
                assert_eq!(batch.docs, expected_docs);

                let mut expected_checkpoint_delta = CheckpointDelta::default();
                for partition in 0..3 {
                    expected_checkpoint_delta.record_partition_delta(
                        PartitionId::from(partition),
                        Position::Beginning,
                        Position::from(2u64),
                    )?;
                }
                assert_eq!(batch.checkpoint_delta, expected_checkpoint_delta);
            };

            let expected_state = json!({
                "group_id": group_id,
                "topic":  topic,
                "assigned_partition_ids": vec![0, 1, 2],
                "current_positions":  vec![(0, 2), (1, 2), (2, 2)],
                "num_active_partitions": 0,
                "num_bytes_processed": 72,
                "num_messages_processed": 9,
                "num_invalid_messages": 3,
            });
            assert_eq!(state, expected_state);
        }
        {
            let (sink, inbox) = create_test_mailbox();
            let checkpoint: Checkpoint = vec![(0, 0), (1, 2)]
                .into_iter()
                .map(|(partition_id, offset)| {
                    (PartitionId::from(partition_id), Position::from(offset))
                })
                .collect();
            let source = KafkaSource::try_new(params.clone(), checkpoint).await?;
            let actor = SourceActor {
                source: Box::new(source),
                batch_sink: sink.clone(),
            };
            let (_mailbox, handle) = universe.spawn_actor(actor).spawn_async();
            let (exit_status, exit_state) = handle.join().await;
            assert!(exit_status.is_success());

            let mut messages = inbox.drain_available_message_for_test();
            assert_eq!(messages.len(), 2);
            assert!(matches!(messages[0], IndexerMessage::Batch(_)));
            assert!(matches!(messages[1], IndexerMessage::EndOfSource));

            if let IndexerMessage::Batch(batch) = &mut messages[0] {
                batch.docs.sort();
                let expected_docs = vec!["Message #002", "Message #200", "Message #202"];
                assert_eq!(batch.docs, expected_docs);

                let mut expected_checkpoint_delta = CheckpointDelta::default();
                expected_checkpoint_delta.record_partition_delta(
                    PartitionId::from(0),
                    Position::from(0u64),
                    Position::from(2u64),
                )?;
                expected_checkpoint_delta.record_partition_delta(
                    PartitionId::from(2),
                    Position::Beginning,
                    Position::from(2u64),
                )?;
                assert_eq!(batch.checkpoint_delta, expected_checkpoint_delta,);
            };
            let expected_exit_state = json!({
                "group_id": group_id,
                "topic":  topic,
                "assigned_partition_ids": vec![0, 1, 2],
                "current_positions":  vec![(0, 2), (2, 2)],
                "num_active_partitions": 0,
                "num_bytes_processed": 36,
                "num_messages_processed": 5,
                "num_invalid_messages": 2,
            });
            assert_eq!(exit_state, expected_exit_state);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_partition_ids() -> anyhow::Result<()> {
        let bootstrap_servers = "localhost:9092".to_string();
        let group_id = append_random_suffix("test-fetch-partitions-consumer-group");
        let topic = append_random_suffix("test-fetch-partitions-topic");

        let admin_client = create_admin_client(&bootstrap_servers)?;
        create_topic(&admin_client, &topic, 2).await?;

        let consumer = create_consumer(&bootstrap_servers, &group_id, true)?;
        assert!(fetch_partition_ids(&consumer, "topic-does-not-exist")
            .await
            .is_err());

        let partition_ids = fetch_partition_ids(&consumer, &topic).await?;
        assert_eq!(&partition_ids, &[0, 1]);
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_watermarks() -> anyhow::Result<()> {
        let bootstrap_servers = "localhost:9092".to_string();
        let group_id = append_random_suffix("test-fetch-watermarks-consumer-group");
        let topic = append_random_suffix("test-fetch-watermarks-topic");

        let admin_client = create_admin_client(&bootstrap_servers)?;
        create_topic(&admin_client, &topic, 2).await?;

        let consumer = create_consumer(&bootstrap_servers, &group_id, true)?;
        // Force metadata update for the consumer. Otherwise, `fetch_watermarks` may return
        // `UnknownPartition` if the broker hasn't received a metadata update since the
        // topic was created. See also https://issues.apache.org/jira/browse/KAFKA-6829.
        consumer.fetch_metadata(Some(&topic), Duration::from_secs(5))?;
        assert!(fetch_watermarks(&consumer, "topic-does-not-exist", &[0])
            .await
            .is_err());
        {
            let watermarks = fetch_watermarks(&consumer, &topic, &[0]).await?;
            let expected_watermarks = vec![(0, (0, 0))].into_iter().collect();
            assert_eq!(watermarks, expected_watermarks);
        }
        {
            let watermarks = fetch_watermarks(&consumer, &topic, &[0, 1]).await?;
            let expected_watermarks = vec![(0, (0, 0)), (1, (0, 0))].into_iter().collect();
            assert_eq!(watermarks, expected_watermarks);
        }
        for partition_id in 0..2 {
            populate_topic(
                &bootstrap_servers,
                &topic,
                1,
                &key_fn,
                &message_fn,
                Some(partition_id),
                None,
            )
            .await?;
        }
        {
            let watermarks = fetch_watermarks(&consumer, &topic, &[0, 1]).await?;
            let expected_watermarks = vec![(0, (0, 1)), (1, (0, 1))].into_iter().collect();
            assert_eq!(watermarks, expected_watermarks);
        }
        Ok(())
    }
}
