use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, RwLock,
    },
    thread,
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use arc_swap::ArcSwap;
use crossbeam_channel::{bounded, Receiver, RecvError, Sender};
use histogram::Histogram;
use jito_core::ofac::is_tx_ofac_related;
use jito_protos::{
    convert::packet_to_proto_packet,
    packet::PacketBatch as ProtoPacketBatch,
    relayer::{
        relayer_server::Relayer, subscribe_packets_response, GetTpuConfigsRequest,
        GetTpuConfigsResponse, SubscribePacketsRequest, SubscribePacketsResponse,
    },
    shared::{Header, Heartbeat, Socket},
};
use jito_rpc::load_balancer::LoadBalancer;
use log::*;
use prost_types::Timestamp;
use solana_core::banking_trace::BankingPacketBatch;
use solana_metrics::datapoint_info;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount, clock::NUM_CONSECUTIVE_LEADER_SLOTS,
    pubkey::Pubkey, saturating_add_assign, transaction::VersionedTransaction,
};
use thiserror::Error;
use tokio::sync::mpsc::{channel, error::TrySendError, Sender as TokioSender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::{health_manager::HealthState, schedule_cache::LeaderScheduleUpdatingHandle};

#[derive(Default)]
struct PacketForwardStats {
    num_packets_forwarded: u64,
    num_packets_dropped: u64,
}

struct RelayerMetrics {
    pub highest_slot: u64,
    pub num_added_connections: u64,
    pub num_removed_connections: u64,
    pub num_current_connections: u64,
    pub num_heartbeats: u64,
    pub max_heartbeat_tick_latency_us: u64,
    pub metrics_latency_us: u64,
    pub num_try_send_channel_full: u64,
    pub packet_latencies_us: Histogram,

    pub crossbeam_delay_packet_receiver_processing_us: Histogram,
    pub crossbeam_subscription_receiver_processing_us: Histogram,
    pub crossbeam_heartbeat_tick_processing_us: Histogram,
    pub crossbeam_metrics_tick_processing_us: Histogram,

    // channel stats
    pub subscription_receiver_max_len: usize,
    pub subscription_receiver_capacity: usize,
    pub delay_packet_receiver_max_len: usize,
    pub delay_packet_receiver_capacity: usize,
    pub packet_subscriptions_total_queued: usize, // sum of all items currently queued
    packet_stats_per_validator: HashMap<Pubkey, PacketForwardStats>,
}

impl RelayerMetrics {
    fn new(subscription_receiver_capacity: usize, delay_packet_receiver_capacity: usize) -> Self {
        RelayerMetrics {
            highest_slot: 0,
            num_added_connections: 0,
            num_removed_connections: 0,
            num_current_connections: 0,
            num_heartbeats: 0,
            max_heartbeat_tick_latency_us: 0,
            metrics_latency_us: 0,
            num_try_send_channel_full: 0,
            packet_latencies_us: Histogram::default(),
            crossbeam_delay_packet_receiver_processing_us: Histogram::default(),
            crossbeam_subscription_receiver_processing_us: Histogram::default(),
            crossbeam_heartbeat_tick_processing_us: Histogram::default(),
            crossbeam_metrics_tick_processing_us: Histogram::default(),
            subscription_receiver_max_len: 0,
            subscription_receiver_capacity,
            delay_packet_receiver_max_len: 0,
            delay_packet_receiver_capacity,
            packet_subscriptions_total_queued: 0,
            packet_stats_per_validator: HashMap::new(),
        }
    }

    fn update_max_len(
        &mut self,
        subscription_receiver_len: usize,
        delay_packet_receiver_len: usize,
    ) {
        self.subscription_receiver_max_len = std::cmp::max(
            self.subscription_receiver_max_len,
            subscription_receiver_len,
        );
        self.delay_packet_receiver_max_len = std::cmp::max(
            self.delay_packet_receiver_max_len,
            delay_packet_receiver_len,
        );
    }

    fn update_packet_subscription_total_capacity(
        &mut self,
        packet_subscriptions: &HashMap<
            Pubkey,
            TokioSender<Result<SubscribePacketsResponse, Status>>,
        >,
    ) {
        let packet_subscriptions_total_queued = packet_subscriptions
            .values()
            .map(|x| RelayerImpl::SUBSCRIBER_QUEUE_CAPACITY - x.capacity())
            .sum::<usize>();
        self.packet_subscriptions_total_queued = packet_subscriptions_total_queued;
    }

    fn increment_packets_forwarded(&mut self, validator_id: &Pubkey, num_packets: u64) {
        self.packet_stats_per_validator
            .entry(*validator_id)
            .and_modify(|entry| saturating_add_assign!(entry.num_packets_forwarded, num_packets))
            .or_insert(PacketForwardStats {
                num_packets_forwarded: num_packets,
                num_packets_dropped: 0,
            });
    }

    fn increment_packets_dropped(&mut self, validator_id: &Pubkey, num_packets: u64) {
        self.packet_stats_per_validator
            .entry(*validator_id)
            .and_modify(|entry| saturating_add_assign!(entry.num_packets_dropped, num_packets))
            .or_insert(PacketForwardStats {
                num_packets_forwarded: 0,
                num_packets_dropped: num_packets,
            });
    }

    fn report(&self) {
        for (pubkey, stats) in &self.packet_stats_per_validator {
            datapoint_info!("relayer_validator_metrics",
                "pubkey" => pubkey.to_string(),
                ("num_packets_forwarded", stats.num_packets_forwarded, i64),
                ("num_packets_dropped", stats.num_packets_dropped, i64),
            );
        }
        datapoint_info!(
            "relayer_metrics",
            ("highest_slot", self.highest_slot, i64),
            ("num_added_connections", self.num_added_connections, i64),
            ("num_removed_connections", self.num_removed_connections, i64),
            ("num_current_connections", self.num_current_connections, i64),
            ("num_heartbeats", self.num_heartbeats, i64),
            (
                "num_try_send_channel_full",
                self.num_try_send_channel_full,
                i64
            ),
            ("metrics_latency_us", self.metrics_latency_us, i64),
            (
                "max_heartbeat_tick_latency_us",
                self.max_heartbeat_tick_latency_us,
                i64
            ),
            // packet latencies
            (
                "packet_latencies_us_min",
                self.packet_latencies_us.minimum().unwrap_or_default(),
                i64
            ),
            (
                "packet_latencies_us_max",
                self.packet_latencies_us.maximum().unwrap_or_default(),
                i64
            ),
            (
                "packet_latencies_us_p50",
                self.packet_latencies_us
                    .percentile(50.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "packet_latencies_us_p90",
                self.packet_latencies_us
                    .percentile(90.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "packet_latencies_us_p99",
                self.packet_latencies_us
                    .percentile(99.0)
                    .unwrap_or_default(),
                i64
            ),
            // crossbeam arm latencies
            (
                "crossbeam_subscription_receiver_processing_us_p50",
                self.crossbeam_subscription_receiver_processing_us
                    .percentile(50.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_subscription_receiver_processing_us_p90",
                self.crossbeam_subscription_receiver_processing_us
                    .percentile(90.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_subscription_receiver_processing_us_p99",
                self.crossbeam_subscription_receiver_processing_us
                    .percentile(99.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_metrics_tick_processing_us_p50",
                self.crossbeam_metrics_tick_processing_us
                    .percentile(50.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_metrics_tick_processing_us_p90",
                self.crossbeam_metrics_tick_processing_us
                    .percentile(90.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_metrics_tick_processing_us_p99",
                self.crossbeam_metrics_tick_processing_us
                    .percentile(99.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_delay_packet_receiver_processing_us_p50",
                self.crossbeam_delay_packet_receiver_processing_us
                    .percentile(50.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_delay_packet_receiver_processing_us_p90",
                self.crossbeam_delay_packet_receiver_processing_us
                    .percentile(90.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_delay_packet_receiver_processing_us_p99",
                self.crossbeam_delay_packet_receiver_processing_us
                    .percentile(99.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_heartbeat_tick_processing_us_p50",
                self.crossbeam_heartbeat_tick_processing_us
                    .percentile(50.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_heartbeat_tick_processing_us_p90",
                self.crossbeam_heartbeat_tick_processing_us
                    .percentile(90.0)
                    .unwrap_or_default(),
                i64
            ),
            (
                "crossbeam_heartbeat_tick_processing_us_p99",
                self.crossbeam_heartbeat_tick_processing_us
                    .percentile(99.0)
                    .unwrap_or_default(),
                i64
            ),
            // channel lengths
            (
                "subscription_receiver_len",
                self.subscription_receiver_max_len,
                i64
            ),
            (
                "subscription_receiver_capacity",
                self.subscription_receiver_capacity,
                i64
            ),
            (
                "delay_packet_receiver_len",
                self.delay_packet_receiver_max_len,
                i64
            ),
            (
                "delay_packet_receiver_capacity",
                self.delay_packet_receiver_capacity,
                i64
            ),
            (
                "packet_subscriptions_total_queued",
                self.packet_subscriptions_total_queued,
                i64
            ),
        );
    }
}

pub struct RelayerPacketBatches {
    pub stamp: Instant,
    pub banking_packet_batch: BankingPacketBatch,
}

pub enum Subscription {
    ValidatorPacketSubscription {
        pubkey: Pubkey,
        sender: TokioSender<Result<SubscribePacketsResponse, Status>>,
    },
}

#[derive(Error, Debug)]
pub enum RelayerError {
    #[error("shutdown")]
    Shutdown(#[from] RecvError),
}

pub type RelayerResult<T> = Result<T, RelayerError>;

type PacketSubscriptions =
    Arc<RwLock<HashMap<Pubkey, TokioSender<Result<SubscribePacketsResponse, Status>>>>>;
pub struct RelayerHandle {
    packet_subscriptions: PacketSubscriptions,
}

impl RelayerHandle {
    pub fn new(packet_subscriptions: &PacketSubscriptions) -> RelayerHandle {
        RelayerHandle {
            packet_subscriptions: packet_subscriptions.clone(),
        }
    }

    pub fn connected_validators(&self) -> Vec<Pubkey> {
        self.packet_subscriptions
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }
}

pub struct RelayerImpl {
    tpu_quic_ports: Vec<u16>,
    tpu_fwd_quic_ports: Vec<u16>,
    public_ip: IpAddr,
    seq: AtomicU64,

    subscription_sender: Sender<Subscription>,
    threads: Vec<JoinHandle<()>>,
    health_state: Arc<RwLock<HealthState>>,
    packet_subscriptions: PacketSubscriptions,
}

impl RelayerImpl {
    pub const SUBSCRIBER_QUEUE_CAPACITY: usize = 50_000;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        highest_slot: Arc<AtomicU64>,
        delay_packet_receiver: Receiver<RelayerPacketBatches>,
        leader_schedule_cache: LeaderScheduleUpdatingHandle,
        public_ip: IpAddr,
        tpu_quic_ports: Vec<u16>,
        tpu_fwd_quic_ports: Vec<u16>,
        health_state: Arc<RwLock<HealthState>>,
        exit: Arc<AtomicBool>,
        ofac_addresses: HashSet<Pubkey>,
        address_lookup_table_cache: &Arc<ArcSwap<hashbrown::HashMap<Pubkey, AddressLookupTableAccount>>>,
        validator_packet_batch_size: usize,
        forward_all: bool,
    ) -> Self {
        const LEADER_LOOKAHEAD: u64 = 2;

        // receiver tracked as relayer_metrics.subscription_receiver_len
        let (subscription_sender, subscription_receiver) =
            bounded(LoadBalancer::SLOT_QUEUE_CAPACITY);

        let packet_subscriptions = Arc::new(RwLock::new(HashMap::with_capacity(1_000)));

        let thread = {
            let address_lookup_table_cache = address_lookup_table_cache.clone();
            let health_state = health_state.clone();
            let packet_subscriptions = packet_subscriptions.clone();
            thread::Builder::new()
                .name("relayer_impl-event_loop_thread".to_string())
                .spawn(move || {
                    let res = Self::run_event_loop(
                        highest_slot,
                        subscription_receiver,
                        delay_packet_receiver,
                        leader_schedule_cache,
                        LEADER_LOOKAHEAD,
                        health_state,
                        exit,
                        &packet_subscriptions,
                        ofac_addresses,
                        &address_lookup_table_cache,
                        validator_packet_batch_size,
                        forward_all,
                    );
                    warn!("RelayerImpl thread exited with result {res:?}")
                })
                .unwrap()
        };

        Self {
            tpu_quic_ports,
            tpu_fwd_quic_ports,
            subscription_sender,
            public_ip,
            threads: vec![thread],
            health_state,
            packet_subscriptions,
            seq: AtomicU64::new(0),
        }
    }

    pub fn handle(&self) -> RelayerHandle {
        RelayerHandle::new(&self.packet_subscriptions)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_event_loop(
        highest_slot: Arc<AtomicU64>,
        subscription_receiver: Receiver<Subscription>,
        delay_packet_receiver: Receiver<RelayerPacketBatches>,
        leader_schedule_cache: LeaderScheduleUpdatingHandle,
        leader_lookahead: u64,
        health_state: Arc<RwLock<HealthState>>,
        exit: Arc<AtomicBool>,
        packet_subscriptions: &PacketSubscriptions,
        ofac_addresses: HashSet<Pubkey>,
        address_lookup_table_cache: &Arc<ArcSwap<hashbrown::HashMap<Pubkey, AddressLookupTableAccount>>>,
        validator_packet_batch_size: usize,
        forward_all: bool,
    ) -> RelayerResult<()> {
        let heartbeat_tick = crossbeam_channel::tick(Duration::from_millis(500));
        let metrics_tick = crossbeam_channel::tick(Duration::from_secs(10));

        let mut relayer_metrics = RelayerMetrics::new(
            subscription_receiver.capacity().unwrap(),
            delay_packet_receiver.capacity().unwrap(),
        );
        let mut last_observed_slot = highest_slot.load(Ordering::Relaxed);
        let mut senders: Vec<(
            Pubkey,
            TokioSender<Result<SubscribePacketsResponse, Status>>,
        )> = vec![];

        while !exit.load(Ordering::Relaxed) {
            crossbeam_channel::select! {
                recv(delay_packet_receiver) -> maybe_packet_batches => {
                    let start = Instant::now();
                    let lookup_table = address_lookup_table_cache.load();
                    let failed_forwards = Self::forward_packets(maybe_packet_batches, &senders, &mut relayer_metrics, &ofac_addresses, lookup_table.as_ref(), validator_packet_batch_size)?;
                    Self::drop_connections(failed_forwards, packet_subscriptions, &mut relayer_metrics);
                    let _ = relayer_metrics.crossbeam_delay_packet_receiver_processing_us.increment(start.elapsed().as_micros() as u64);
                },
                recv(subscription_receiver) -> maybe_subscription => {
                    let start = Instant::now();
                    Self::handle_subscription(maybe_subscription, packet_subscriptions, &mut relayer_metrics)?;
                    let _ = relayer_metrics.crossbeam_subscription_receiver_processing_us.increment(start.elapsed().as_micros() as u64);
                }
                recv(heartbeat_tick) -> time_generated => {
                    let start = Instant::now();
                    if let Ok(time_generated) = time_generated {
                        relayer_metrics.max_heartbeat_tick_latency_us = std::cmp::max(relayer_metrics.max_heartbeat_tick_latency_us, Instant::now().duration_since(time_generated).as_micros() as u64);
                    }

                    // heartbeat if state is healthy, drop all connections on unhealthy
                    let pubkeys_to_drop = match *health_state.read().unwrap() {
                        HealthState::Healthy => {
                            Self::handle_heartbeat(
                                packet_subscriptions,
                                &mut relayer_metrics,
                            )
                        },
                        HealthState::Unhealthy => packet_subscriptions.read().unwrap().keys().copied().collect(),
                    };
                    Self::drop_connections(pubkeys_to_drop, packet_subscriptions, &mut relayer_metrics);
                    let _ = relayer_metrics.crossbeam_heartbeat_tick_processing_us.increment(start.elapsed().as_micros() as u64);
                }
                recv(metrics_tick) -> time_generated => {
                    let start = Instant::now();
                    let l_packet_subscriptions = packet_subscriptions.read().unwrap();
                    relayer_metrics.num_current_connections = l_packet_subscriptions.len() as u64;
                    relayer_metrics.update_packet_subscription_total_capacity(&l_packet_subscriptions);
                    drop(l_packet_subscriptions);

                    if let Ok(time_generated) = time_generated {
                        relayer_metrics.metrics_latency_us = time_generated.elapsed().as_micros() as u64;
                    }
                    let _ = relayer_metrics.crossbeam_metrics_tick_processing_us.increment(start.elapsed().as_micros() as u64);

                    relayer_metrics.report();
                    relayer_metrics = RelayerMetrics::new(
                        subscription_receiver.capacity().unwrap(),
                        delay_packet_receiver.capacity().unwrap(),
                    );
                }
            }

            // update senders every new slot
            let new_slot = highest_slot.load(Ordering::Relaxed);
            if last_observed_slot != new_slot {
                last_observed_slot = new_slot;
                let packet_subscriptions = packet_subscriptions.read().unwrap();
                if forward_all {
                    senders = packet_subscriptions
                        .iter()
                        .map(|(pk, sender)| (*pk, sender.clone()))
                        .collect()
                } else {
                    let slot_leaders =
                        new_slot..new_slot + leader_lookahead * NUM_CONSECUTIVE_LEADER_SLOTS;
                    let schedule = leader_schedule_cache.get_schedule().load();
                    senders = slot_leaders
                        .filter_map(|s| schedule.get(&s))
                        .filter_map(|pubkey| {
                            Some((*pubkey, packet_subscriptions.get(pubkey)?.clone()))
                        })
                        .collect()
                }
            }
            relayer_metrics
                .update_max_len(subscription_receiver.len(), delay_packet_receiver.len());
        }
        Ok(())
    }

    fn drop_connections(
        disconnected_pubkeys: Vec<Pubkey>,
        subscriptions: &PacketSubscriptions,
        relayer_metrics: &mut RelayerMetrics,
    ) {
        relayer_metrics.num_removed_connections += disconnected_pubkeys.len() as u64;

        let mut l_subscriptions = subscriptions.write().unwrap();
        for disconnected in disconnected_pubkeys {
            if let Some(sender) = l_subscriptions.remove(&disconnected) {
                datapoint_info!(
                    "relayer_removed_subscription",
                    ("pubkey", disconnected.to_string(), String)
                );
                drop(sender);
            }
        }
    }

    fn handle_heartbeat(
        subscriptions: &PacketSubscriptions,
        relayer_metrics: &mut RelayerMetrics,
    ) -> Vec<Pubkey> {
        let failed_pubkey_updates = subscriptions
            .read()
            .unwrap()
            .iter()
            .filter_map(|(pubkey, sender)| {
                // try send because it's a bounded channel and we don't want to block if the channel is full
                match sender.try_send(Ok(SubscribePacketsResponse {
                    header: None,
                    msg: Some(subscribe_packets_response::Msg::Heartbeat(Heartbeat {
                        count: relayer_metrics.num_heartbeats,
                    })),
                })) {
                    Ok(_) => {}
                    Err(TrySendError::Closed(_)) => return Some(*pubkey),
                    Err(TrySendError::Full(_)) => {
                        relayer_metrics.num_try_send_channel_full += 1;
                        warn!("heartbeat channel is full for: {:?}", pubkey);
                    }
                }
                None
            })
            .collect();

        relayer_metrics.num_heartbeats += 1;

        failed_pubkey_updates
    }

    /// Returns pubkeys of subscribers that failed to send
    fn forward_packets(
        maybe_packet_batches: Result<RelayerPacketBatches, RecvError>,
        senders: &Vec<(
            Pubkey,
            TokioSender<Result<SubscribePacketsResponse, Status>>,
        )>,
        relayer_metrics: &mut RelayerMetrics,
        ofac_addresses: &HashSet<Pubkey>,
        address_lookup_table_cache: &hashbrown::HashMap<Pubkey, AddressLookupTableAccount>,
        validator_packet_batch_size: usize,
    ) -> RelayerResult<Vec<Pubkey>> {
        let packet_batches = maybe_packet_batches?;

        let _ = relayer_metrics
            .packet_latencies_us
            .increment(packet_batches.stamp.elapsed().as_micros() as u64);

        // remove discards + check for OFAC before forwarding
        let packets: Vec<_> = packet_batches
            .banking_packet_batch
            .0
            .iter()
            .flat_map(|batch| batch.iter().filter(|p| !p.meta().discard()))
            .filter_map(|packet| {
                if ofac_addresses.is_empty() {
                    return Some(packet);
                }
                let tx: VersionedTransaction = packet.deserialize_slice(..).ok()?;
                if is_tx_ofac_related(&tx, ofac_addresses, address_lookup_table_cache) {
                    return None;
                }
                Some(packet)
            })
            .filter_map(packet_to_proto_packet)
            .collect();
        if packets.is_empty() {
            return Ok(vec![]);
        }

        let mut proto_packet_batches =
            Vec::with_capacity(packets.len() / validator_packet_batch_size);
        for packet_chunk in packets.chunks(validator_packet_batch_size) {
            proto_packet_batches.push(ProtoPacketBatch {
                packets: packet_chunk.to_vec(),
            });
        }

        let mut failed_forwards = Vec::new();
        for batch in &proto_packet_batches {
            // NOTE: this is important to avoid divide-by-0 inside the validator if packets
            // get routed to sigverify under the assumption there's > 0 packets in the batch
            if batch.packets.is_empty() {
                continue;
            }
            let now = Timestamp::from(SystemTime::now());
            for (pubkey, sender) in senders {
                // try send because it's a bounded channel and we don't want to block if the channel is full
                match sender.try_send(Ok(SubscribePacketsResponse {
                    header: Some(Header {
                        ts: Some(now.clone()),
                    }),
                    msg: Some(subscribe_packets_response::Msg::Batch(batch.clone())),
                })) {
                    Ok(_) => {
                        relayer_metrics
                            .increment_packets_forwarded(pubkey, batch.packets.len() as u64);
                    }
                    Err(TrySendError::Full(_)) => {
                        error!("packet channel is full for pubkey: {:?}", pubkey);
                        relayer_metrics
                            .increment_packets_dropped(pubkey, batch.packets.len() as u64);
                    }
                    Err(TrySendError::Closed(_)) => {
                        error!("channel is closed for pubkey: {:?}", pubkey);
                        failed_forwards.push(*pubkey);
                        break;
                    }
                }
            }
        }
        Ok(failed_forwards)
    }

    fn handle_subscription(
        maybe_subscription: Result<Subscription, RecvError>,
        subscriptions: &PacketSubscriptions,
        relayer_metrics: &mut RelayerMetrics,
    ) -> RelayerResult<()> {
        match maybe_subscription? {
            Subscription::ValidatorPacketSubscription { pubkey, sender } => {
                match subscriptions.write().unwrap().entry(pubkey) {
                    Entry::Vacant(entry) => {
                        entry.insert(sender);

                        relayer_metrics.num_added_connections += 1;
                        datapoint_info!(
                            "relayer_new_subscription",
                            ("pubkey", pubkey.to_string(), String)
                        );
                    }
                    Entry::Occupied(mut entry) => {
                        datapoint_info!(
                            "relayer_duplicate_subscription",
                            ("pubkey", pubkey.to_string(), String)
                        );
                        error!("already connected, dropping old connection: {pubkey:?}");
                        entry.insert(sender);
                    }
                }
            }
        }
        Ok(())
    }

    /// Prevent validators from authenticating if the relayer is unhealthy
    fn check_health(health_state: &Arc<RwLock<HealthState>>) -> Result<(), Status> {
        if *health_state.read().unwrap() != HealthState::Healthy {
            Err(Status::internal("relayer is unhealthy"))
        } else {
            Ok(())
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for t in self.threads {
            t.join()?;
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl Relayer for RelayerImpl {
    /// Validator calls this to get the public IP of the relayers TPU and TPU forward sockets.
    async fn get_tpu_configs(
        &self,
        _: Request<GetTpuConfigsRequest>,
    ) -> Result<Response<GetTpuConfigsResponse>, Status> {
        let seq = self.seq.fetch_add(1, Ordering::Acquire);
        return Ok(Response::new(GetTpuConfigsResponse {
            tpu: Some(Socket {
                ip: self.public_ip.to_string(),
                port: (self.tpu_quic_ports[seq as usize % self.tpu_quic_ports.len()] - 6) as i64,
            }),
            tpu_forward: Some(Socket {
                ip: self.public_ip.to_string(),
                port: (self.tpu_fwd_quic_ports[seq as usize % self.tpu_fwd_quic_ports.len()] - 6)
                    as i64,
            }),
        }));
    }

    type SubscribePacketsStream = ReceiverStream<Result<SubscribePacketsResponse, Status>>;

    /// Validator calls this to subscribe to packets
    async fn subscribe_packets(
        &self,
        request: Request<SubscribePacketsRequest>,
    ) -> Result<Response<Self::SubscribePacketsStream>, Status> {
        Self::check_health(&self.health_state)?;

        let pubkey: &Pubkey = request
            .extensions()
            .get()
            .ok_or_else(|| Status::internal("internal error fetching public key"))?;

        let (sender, receiver) = channel(RelayerImpl::SUBSCRIBER_QUEUE_CAPACITY);
        self.subscription_sender
            .send(Subscription::ValidatorPacketSubscription {
                pubkey: *pubkey,
                sender,
            })
            .map_err(|_| Status::internal("internal error adding subscription"))?;
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}
