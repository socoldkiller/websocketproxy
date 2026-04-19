use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::{
    Router,
    extract::{
        ConnectInfo, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, header},
    response::IntoResponse,
    routing::get,
};
use bytes::Bytes;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use tun::{AbstractDevice, Layer};

type ClientId = u64;
type Frame = Bytes;

const CLIENT_OUTBOUND_QUEUE_CAPACITY: usize = 128;
const CLIENT_OUTBOUND_DRAIN_BATCH_SIZE: usize = 32;
const TRAFFIC_RECENT_WINDOW_SECS: u64 = 5;
const TRAFFIC_RECENT_WINDOW_SIZE: usize = 5;
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Clone, Copy, Eq, PartialEq)]
enum SessionControl {
    Continue,
    Disconnect,
}

#[derive(Parser, Debug, Clone)]
#[command(
    author,
    version,
    about = "Rust WebSocket Ethernet relay without rate limiting"
)]
struct Cli {
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:80")]
    listen_addr: SocketAddr,

    #[arg(long, env = "TAP_NAME", default_value = "tap0")]
    tap_name: String,

    #[arg(long, env = "TAP_MTU", default_value_t = 1500)]
    tap_mtu: u16,
}

#[derive(Serialize)]
struct TrafficSnapshot {
    uptime_seconds: u64,
    recent_window_seconds: u64,
    connected_clients: usize,
    websocket_rx: DirectionSnapshot,
    websocket_tx: DirectionSnapshot,
    tap_rx: DirectionSnapshot,
    tap_tx: DirectionSnapshot,
    current_bps: u64,
}

#[derive(Serialize)]
struct DirectionSnapshot {
    frames: u64,
    bytes: u64,
    recent_frames: u64,
    recent_bytes: u64,
    recent_bps: u64,
}

#[derive(Clone, Copy, Default)]
struct DirectionTotals {
    frames: u64,
    bytes: u64,
}

impl DirectionTotals {
    const fn saturating_sub(self, older: Self) -> Self {
        Self {
            frames: self.frames.saturating_sub(older.frames),
            bytes: self.bytes.saturating_sub(older.bytes),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct TrafficTotals {
    websocket_rx: DirectionTotals,
    websocket_tx: DirectionTotals,
    tap_rx: DirectionTotals,
    tap_tx: DirectionTotals,
}

impl TrafficTotals {
    const fn saturating_sub(self, older: Self) -> Self {
        Self {
            websocket_rx: self.websocket_rx.saturating_sub(older.websocket_rx),
            websocket_tx: self.websocket_tx.saturating_sub(older.websocket_tx),
            tap_rx: self.tap_rx.saturating_sub(older.tap_rx),
            tap_tx: self.tap_tx.saturating_sub(older.tap_tx),
        }
    }

    const fn total_bytes(self) -> u64 {
        self.websocket_rx
            .bytes
            .saturating_add(self.websocket_tx.bytes)
            .saturating_add(self.tap_rx.bytes)
            .saturating_add(self.tap_tx.bytes)
    }
}

struct DirectionCounter {
    frames: AtomicU64,
    bytes: AtomicU64,
}

impl DirectionCounter {
    const fn new() -> Self {
        Self {
            frames: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    fn record(&self, frames: u64, bytes: u64) {
        self.frames.fetch_add(frames, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DirectionTotals {
        DirectionTotals {
            frames: self.frames.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

// Hot path only increments atomics; a background task samples these once per second.
struct TrafficCounters {
    websocket_rx: DirectionCounter,
    websocket_tx: DirectionCounter,
    tap_rx: DirectionCounter,
    tap_tx: DirectionCounter,
}

impl TrafficCounters {
    const fn new() -> Self {
        Self {
            websocket_rx: DirectionCounter::new(),
            websocket_tx: DirectionCounter::new(),
            tap_rx: DirectionCounter::new(),
            tap_tx: DirectionCounter::new(),
        }
    }

    fn record_websocket_rx(&self, bytes: usize) {
        self.websocket_rx.record(1, saturating_usize_to_u64(bytes));
    }

    fn record_websocket_tx_batch(&self, frames: u64, bytes: u64) {
        self.websocket_tx.record(frames, bytes);
    }

    fn record_tap_rx(&self, bytes: usize) {
        self.tap_rx.record(1, saturating_usize_to_u64(bytes));
    }

    fn record_tap_tx(&self, bytes: usize) {
        self.tap_tx.record(1, saturating_usize_to_u64(bytes));
    }

    fn snapshot(&self) -> TrafficTotals {
        TrafficTotals {
            websocket_rx: self.websocket_rx.snapshot(),
            websocket_tx: self.websocket_tx.snapshot(),
            tap_rx: self.tap_rx.snapshot(),
            tap_tx: self.tap_tx.snapshot(),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct TrafficSample {
    second: u64,
    totals: TrafficTotals,
}

struct TrafficHistory {
    samples: [TrafficSample; TRAFFIC_RECENT_WINDOW_SIZE + 1],
}

impl TrafficHistory {
    fn new() -> Self {
        Self {
            samples: [TrafficSample::default(); TRAFFIC_RECENT_WINDOW_SIZE + 1],
        }
    }

    fn record(&mut self, second: u64, totals: TrafficTotals) {
        let slot = usize::try_from(second).unwrap_or(usize::MAX) % self.samples.len();
        self.samples[slot] = TrafficSample { second, totals };
    }

    fn baseline(&self, target_second: u64) -> TrafficTotals {
        let mut best_second = 0_u64;
        let mut best_totals = TrafficTotals::default();

        for sample in &self.samples {
            let sample = *sample;
            if sample.second <= target_second && sample.second >= best_second {
                best_second = sample.second;
                best_totals = sample.totals;
            }
        }

        best_totals
    }
}

struct TrafficStats {
    started_at: Instant,
    counters: TrafficCounters,
    history: std::sync::Mutex<TrafficHistory>,
}

impl TrafficStats {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            counters: TrafficCounters::new(),
            history: std::sync::Mutex::new(TrafficHistory::new()),
        }
    }

    fn record_websocket_rx(&self, bytes: usize) {
        self.counters.record_websocket_rx(bytes);
    }

    fn record_websocket_tx_batch(&self, frames: u64, bytes: u64) {
        self.counters.record_websocket_tx_batch(frames, bytes);
    }

    fn record_tap_rx(&self, bytes: usize) {
        self.counters.record_tap_rx(bytes);
    }

    fn record_tap_tx(&self, bytes: usize) {
        self.counters.record_tap_tx(bytes);
    }

    fn sample(&self) {
        self.sample_at(self.elapsed_seconds());
    }

    fn sample_at(&self, second: u64) {
        let totals = self.counters.snapshot();
        self.history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(second, totals);
    }

    fn snapshot(&self, connected_clients: usize) -> TrafficSnapshot {
        self.snapshot_at(self.elapsed_seconds(), connected_clients)
    }

    fn snapshot_at(&self, second: u64, connected_clients: usize) -> TrafficSnapshot {
        let recent_window_seconds = second.clamp(1, TRAFFIC_RECENT_WINDOW_SECS);
        let current = self.counters.snapshot();
        let baseline_second = second.saturating_sub(recent_window_seconds);
        let baseline = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .baseline(baseline_second);
        let recent = current.saturating_sub(baseline);
        let current_bps = recent.total_bytes() / recent_window_seconds;

        TrafficSnapshot {
            uptime_seconds: second,
            recent_window_seconds,
            connected_clients,
            websocket_rx: DirectionSnapshot {
                frames: current.websocket_rx.frames,
                bytes: current.websocket_rx.bytes,
                recent_frames: recent.websocket_rx.frames,
                recent_bytes: recent.websocket_rx.bytes,
                recent_bps: recent.websocket_rx.bytes / recent_window_seconds,
            },
            websocket_tx: DirectionSnapshot {
                frames: current.websocket_tx.frames,
                bytes: current.websocket_tx.bytes,
                recent_frames: recent.websocket_tx.frames,
                recent_bytes: recent.websocket_tx.bytes,
                recent_bps: recent.websocket_tx.bytes / recent_window_seconds,
            },
            tap_rx: DirectionSnapshot {
                frames: current.tap_rx.frames,
                bytes: current.tap_rx.bytes,
                recent_frames: recent.tap_rx.frames,
                recent_bytes: recent.tap_rx.bytes,
                recent_bps: recent.tap_rx.bytes / recent_window_seconds,
            },
            tap_tx: DirectionSnapshot {
                frames: current.tap_tx.frames,
                bytes: current.tap_tx.bytes,
                recent_frames: recent.tap_tx.frames,
                recent_bytes: recent.tap_tx.bytes,
                recent_bps: recent.tap_tx.bytes / recent_window_seconds,
            },
            current_bps,
        }
    }

    fn elapsed_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

fn format_traffic_prometheus(snapshot: &TrafficSnapshot) -> String {
    let mut output = String::with_capacity(1024);

    write_metric(
        &mut output,
        "websockproxy_uptime_seconds",
        "Process uptime in seconds.",
        "gauge",
        snapshot.uptime_seconds,
    );
    write_metric(
        &mut output,
        "websockproxy_recent_window_seconds",
        "Recent sampling window in seconds.",
        "gauge",
        snapshot.recent_window_seconds,
    );
    write_metric(
        &mut output,
        "websockproxy_connected_clients",
        "Connected websocket clients.",
        "gauge",
        snapshot.connected_clients,
    );
    write_metric(
        &mut output,
        "websockproxy_current_bytes_per_second",
        "Recent aggregate traffic rate in bytes per second.",
        "gauge",
        snapshot.current_bps,
    );

    write_direction_metric_family(
        &mut output,
        "websockproxy_traffic_direction_frames_total",
        "Total frames observed per traffic direction.",
        "counter",
        [
            ("websocket_rx", snapshot.websocket_rx.frames),
            ("websocket_tx", snapshot.websocket_tx.frames),
            ("tap_rx", snapshot.tap_rx.frames),
            ("tap_tx", snapshot.tap_tx.frames),
        ],
    );
    write_direction_metric_family(
        &mut output,
        "websockproxy_traffic_direction_bytes_total",
        "Total bytes observed per traffic direction.",
        "counter",
        [
            ("websocket_rx", snapshot.websocket_rx.bytes),
            ("websocket_tx", snapshot.websocket_tx.bytes),
            ("tap_rx", snapshot.tap_rx.bytes),
            ("tap_tx", snapshot.tap_tx.bytes),
        ],
    );
    write_direction_metric_family(
        &mut output,
        "websockproxy_traffic_direction_recent_frames",
        "Frames observed per traffic direction over the recent window.",
        "gauge",
        [
            ("websocket_rx", snapshot.websocket_rx.recent_frames),
            ("websocket_tx", snapshot.websocket_tx.recent_frames),
            ("tap_rx", snapshot.tap_rx.recent_frames),
            ("tap_tx", snapshot.tap_tx.recent_frames),
        ],
    );
    write_direction_metric_family(
        &mut output,
        "websockproxy_traffic_direction_recent_bytes",
        "Bytes observed per traffic direction over the recent window.",
        "gauge",
        [
            ("websocket_rx", snapshot.websocket_rx.recent_bytes),
            ("websocket_tx", snapshot.websocket_tx.recent_bytes),
            ("tap_rx", snapshot.tap_rx.recent_bytes),
            ("tap_tx", snapshot.tap_tx.recent_bytes),
        ],
    );
    write_direction_metric_family(
        &mut output,
        "websockproxy_traffic_direction_recent_bytes_per_second",
        "Recent bytes per second observed per traffic direction.",
        "gauge",
        [
            ("websocket_rx", snapshot.websocket_rx.recent_bps),
            ("websocket_tx", snapshot.websocket_tx.recent_bps),
            ("tap_rx", snapshot.tap_rx.recent_bps),
            ("tap_tx", snapshot.tap_tx.recent_bps),
        ],
    );

    output
}

fn write_metric<T: fmt::Display>(
    output: &mut String,
    name: &str,
    help: &str,
    kind: &str,
    value: T,
) {
    use std::fmt::Write as _;

    let _ = writeln!(output, "# HELP {name} {help}");
    let _ = writeln!(output, "# TYPE {name} {kind}");
    let _ = writeln!(output, "{name} {value}");
}

fn write_direction_metric_family(
    output: &mut String,
    name: &str,
    help: &str,
    kind: &str,
    samples: [(&str, u64); 4],
) {
    use std::fmt::Write as _;

    let _ = writeln!(output, "# HELP {name} {help}");
    let _ = writeln!(output, "# TYPE {name} {kind}");

    for (direction, value) in samples {
        let _ = writeln!(
            output,
            "{name}{{direction=\"{}\"}} {value}",
            escape_prometheus_label_value(direction)
        );
    }
}

fn escape_prometheus_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(ch),
        }
    }

    escaped
}

#[derive(Clone)]
struct ClientHandle {
    tx: mpsc::Sender<Frame>,
    mac: Option<MacAddress>,
}

#[derive(Clone, Default)]
struct SwitchTable {
    clients: HashMap<ClientId, ClientHandle>,
    mac_index: HashMap<MacAddress, ClientId>,
}

struct AppState {
    next_client_id: AtomicU64,
    tap_tx: mpsc::UnboundedSender<Frame>,
    traffic: TrafficStats,
    table: ArcSwap<SwitchTable>,
}

impl AppState {
    fn new(tap_tx: mpsc::UnboundedSender<Frame>) -> Self {
        Self {
            next_client_id: AtomicU64::new(1),
            tap_tx,
            traffic: TrafficStats::new(),
            table: ArcSwap::from_pointee(SwitchTable::default()),
        }
    }

    fn next_client_id(&self) -> ClientId {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }

    fn client_count(&self) -> usize {
        self.table.load().clients.len()
    }

    fn register_client(&self, client_id: ClientId, tx: &mpsc::Sender<Frame>) {
        self.table.rcu(|table| {
            let mut next = table.as_ref().clone();
            next.clients.insert(
                client_id,
                ClientHandle {
                    tx: tx.clone(),
                    mac: None,
                },
            );
            next
        });
    }

    fn record_websocket_rx(&self, bytes: usize) {
        self.traffic.record_websocket_rx(bytes);
    }

    fn record_websocket_tx_batch(&self, frames: u64, bytes: u64) {
        self.traffic.record_websocket_tx_batch(frames, bytes);
    }

    fn record_tap_rx(&self, bytes: usize) {
        self.traffic.record_tap_rx(bytes);
    }

    fn record_tap_tx(&self, bytes: usize) {
        self.traffic.record_tap_tx(bytes);
    }

    fn traffic_sample(&self) {
        self.traffic.sample();
    }

    fn traffic_snapshot(&self) -> TrafficSnapshot {
        self.traffic.snapshot(self.client_count())
    }

    fn unregister_client(&self, client_id: ClientId) {
        let table = self.table.load();
        if !table.clients.contains_key(&client_id) {
            return;
        }

        self.table.rcu(|table| {
            if !table.clients.contains_key(&client_id) {
                return table.as_ref().clone();
            }

            let mut next = table.as_ref().clone();
            if let Some(mac) = next
                .clients
                .remove(&client_id)
                .and_then(|client| client.mac)
            {
                if next.mac_index.get(&mac) == Some(&client_id) {
                    next.mac_index.remove(&mac);
                }
            }

            next
        });
    }

    fn learn_mac(&self, client_id: ClientId, mac: MacAddress) -> bool {
        let table = self.table.load();
        let Some(client) = table.clients.get(&client_id) else {
            return false;
        };

        if client.mac == Some(mac) {
            return false;
        }

        self.table.rcu(|table| {
            let Some(client) = table.clients.get(&client_id) else {
                return table.as_ref().clone();
            };

            if client.mac == Some(mac) {
                return table.as_ref().clone();
            }

            let mut next = table.as_ref().clone();

            let previous_mac = match next.clients.get_mut(&client_id) {
                Some(client) => client.mac.replace(mac),
                None => return table.as_ref().clone(),
            };

            if let Some(old_mac) = previous_mac {
                if next.mac_index.get(&old_mac) == Some(&client_id) {
                    next.mac_index.remove(&old_mac);
                }
            }

            if let Some(previous_owner) = next.mac_index.insert(mac, client_id) {
                if previous_owner != client_id {
                    if let Some(previous_client) = next.clients.get_mut(&previous_owner) {
                        previous_client.mac = None;
                    }
                }
            }

            next
        });

        true
    }

    fn sender_for_mac(&self, mac: MacAddress) -> Option<(ClientId, mpsc::Sender<Frame>)> {
        let table = self.table.load();
        let client_id = *table.mac_index.get(&mac)?;
        table
            .clients
            .get(&client_id)
            .map(|client| (client_id, client.tx.clone()))
    }

    fn send_to_tap(&self, frame: Frame) -> Result<(), mpsc::error::SendError<Frame>> {
        self.tap_tx.send(frame)
    }
}

fn saturating_usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
struct MacAddress([u8; 6]);

impl MacAddress {
    const BROADCAST: Self = Self([0xff; 6]);

    fn from_slice(bytes: &[u8]) -> Option<Self> {
        Some(Self(bytes.try_into().ok()?))
    }

    fn destination(frame: &[u8]) -> Option<Self> {
        Self::from_slice(frame.get(..6)?)
    }

    fn source(frame: &[u8]) -> Option<Self> {
        Self::from_slice(frame.get(6..12)?)
    }

    fn is_broadcast_or_multicast(self) -> bool {
        self == Self::BROADCAST || (self.0[0] & 0x01) == 1
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let (tap_tx, tap_rx) = mpsc::unbounded_channel();
    let state = Arc::new(AppState::new(tap_tx));

    let tap_state = Arc::clone(&state);
    let tap_cli = cli.clone();
    let tap_task = tokio::spawn(async move { run_tap_task(tap_cli, tap_state, tap_rx).await });

    let traffic_state = Arc::clone(&state);
    let _traffic_task = tokio::spawn(async move { run_traffic_task(traffic_state).await });

    let app = Router::new()
        .route("/", get(ws_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(cli.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", cli.listen_addr))?;

    info!(listen = %cli.listen_addr, "websocket relay listening");

    tokio::select! {
        result = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()) => {
            result.context("axum server exited")?;
        }
        result = tap_task => {
            result.context("tap task join error")??;
        }
    }

    Ok(())
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let peer = forwarded_for(&headers).unwrap_or_else(|| addr.ip().to_string());
    ws.on_upgrade(move |socket| client_session(socket, state, peer))
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        format_traffic_prometheus(&state.traffic_snapshot()),
    )
}

async fn run_traffic_task(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        state.traffic_sample();
    }
}

fn forwarded_for(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

async fn client_session(socket: WebSocket, state: Arc<AppState>, peer: String) {
    let client_id = state.next_client_id();
    let (client_tx, mut client_rx) = mpsc::channel(CLIENT_OUTBOUND_QUEUE_CAPACITY);
    state.register_client(client_id, &client_tx);

    info!(client_id, peer = %peer, "client connected");

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let peer_for_writer = peer.clone();
    let writer_state = Arc::clone(&state);
    let writer_task = tokio::spawn(async move {
        let mut batch = Vec::with_capacity(CLIENT_OUTBOUND_DRAIN_BATCH_SIZE);

        while let Some(frame) = client_rx.recv().await {
            batch.clear();
            batch.push(frame);

            while batch.len() < CLIENT_OUTBOUND_DRAIN_BATCH_SIZE {
                match client_rx.try_recv() {
                    Ok(frame) => batch.push(frame),
                    Err(
                        mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected,
                    ) => break,
                }
            }

            let mut sent_frames = 0_u64;
            let mut sent_bytes = 0_u64;

            for frame in batch.split_off(0) {
                let frame_len = frame.len();
                if ws_sender.send(Message::Binary(frame)).await.is_err() {
                    if sent_frames > 0 {
                        writer_state.record_websocket_tx_batch(sent_frames, sent_bytes);
                    }
                    debug!(client_id, peer = %peer_for_writer, "client writer stopped");
                    return;
                }
                sent_frames = sent_frames.saturating_add(1);
                sent_bytes = sent_bytes.saturating_add(saturating_usize_to_u64(frame_len));
            }

            if sent_frames > 0 {
                writer_state.record_websocket_tx_batch(sent_frames, sent_bytes);
            }
        }
    });

    while let Some(message) = ws_receiver.next().await {
        if handle_client_message(&state, client_id, message) == SessionControl::Disconnect {
            break;
        }
    }

    writer_task.abort();
    state.unregister_client(client_id);
    info!(client_id, peer = %peer, "client disconnected");
}

fn handle_client_message(
    state: &AppState,
    client_id: ClientId,
    message: Result<Message, axum::Error>,
) -> SessionControl {
    match message {
        Ok(Message::Binary(frame)) => {
            state.record_websocket_rx(frame.len());
            handle_client_frame(state, client_id, frame);
            SessionControl::Continue
        }
        Ok(Message::Close(_)) | Err(_) => SessionControl::Disconnect,
        Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_)) => SessionControl::Continue,
    }
}

fn handle_client_frame(state: &AppState, client_id: ClientId, frame: Frame) {
    let Some((destination, source)) = ethernet_addrs(&frame) else {
        warn!(client_id, "dropping short websocket frame");
        return;
    };

    if state.learn_mac(client_id, source) {
        info!(client_id, mac = %source, "learned client MAC");
    }

    if destination.is_broadcast_or_multicast() {
        let table = state.table.load();
        for (target_client_id, target) in &table.clients {
            if *target_client_id == client_id {
                continue;
            }

            if let Err(error) = target.tx.try_send(frame.clone()) {
                debug!(
                    client_id,
                    target_client_id = *target_client_id,
                    %error,
                    "failed to queue broadcast frame for client"
                );
            }
        }

        if let Err(error) = state.send_to_tap(frame) {
            warn!(client_id, %error, "failed to forward broadcast frame to TAP");
        }

        return;
    }

    if let Some((target_client_id, target)) = state.sender_for_mac(destination) {
        if target_client_id == client_id {
            debug!(client_id, destination = %destination, "dropping frame addressed back to ingress client");
            return;
        }

        if let Err(error) = target.try_send(frame) {
            debug!(client_id, %error, destination = %destination, "failed to queue unicast frame for client");
        }
        return;
    }

    let table = state.table.load();
    for (target_client_id, target) in &table.clients {
        if *target_client_id == client_id {
            continue;
        }

        if let Err(error) = target.tx.try_send(frame.clone()) {
            debug!(
                client_id,
                target_client_id = *target_client_id,
                %error,
                destination = %destination,
                "failed to queue flooded frame for client"
            );
        }
    }

    if let Err(error) = state.send_to_tap(frame) {
        warn!(client_id, %error, destination = %destination, "failed to forward frame to TAP");
    }
}

async fn run_tap_task(
    cli: Cli,
    state: Arc<AppState>,
    mut tap_rx: mpsc::UnboundedReceiver<Frame>,
) -> Result<()> {
    let mut config = tun::Configuration::default();
    config
        .tun_name(&cli.tap_name)
        .mtu(cli.tap_mtu)
        .layer(Layer::L2)
        .up();

    #[cfg(target_os = "linux")]
    config.platform_config(|config| {
        config.ensure_root_privileges(true);
    });

    let device = tun::create_as_async(&config).context("failed to create TAP device")?;
    let tap_name = device.tun_name().unwrap_or_else(|_| cli.tap_name.clone());

    info!(
        tap = %tap_name,
        mtu = cli.tap_mtu,
        "tap device ready"
    );

    let mut buffer = vec![0_u8; usize::from(cli.tap_mtu) + 18];

    loop {
        tokio::select! {
            outgoing = tap_rx.recv() => {
                if let Some(frame) = outgoing {
                    let frame_len = frame.len();
                    device.send(frame.as_ref()).await.context("failed to write frame to TAP")?;
                    state.record_tap_tx(frame_len);
                } else {
                    warn!("tap writer channel closed");
                    return Ok(());
                }
            }
            incoming = device.recv(&mut buffer) => {
                let size = incoming.context("failed to read frame from TAP")?;
                if size == 0 {
                    continue;
                }
                state.record_tap_rx(size);
                handle_tap_frame(&state, Bytes::copy_from_slice(&buffer[..size]));
            }
        }
    }
}

fn handle_tap_frame(state: &AppState, frame: Frame) {
    let Some((destination, _source)) = ethernet_addrs(&frame) else {
        warn!("dropping short TAP frame");
        return;
    };

    if destination.is_broadcast_or_multicast() {
        let table = state.table.load();
        for (client_id, target) in &table.clients {
            if let Err(error) = target.tx.try_send(frame.clone()) {
                debug!(
                    client_id = *client_id,
                    %error,
                    "failed to queue TAP broadcast frame for client"
                );
            }
        }
        return;
    }

    if let Some((_client_id, target)) = state.sender_for_mac(destination) {
        if let Err(error) = target.try_send(frame) {
            debug!(
                destination = %destination,
                %error,
                "failed to queue TAP unicast frame for client"
            );
        }
        return;
    }

    let table = state.table.load();
    for (client_id, target) in &table.clients {
        if let Err(error) = target.tx.try_send(frame.clone()) {
            debug!(
                client_id = *client_id,
                destination = %destination,
                %error,
                "failed to queue flooded TAP frame for client"
            );
        }
    }
}

fn ethernet_addrs(frame: &[u8]) -> Option<(MacAddress, MacAddress)> {
    Some((MacAddress::destination(frame)?, MacAddress::source(frame)?))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        AppState, MacAddress, TrafficStats, ethernet_addrs, format_traffic_prometheus,
        handle_client_frame, handle_tap_frame,
    };
    use bytes::Bytes;
    use tokio::sync::mpsc::{self, error::TryRecvError};

    #[test]
    fn parses_destination_and_source_mac() {
        let frame = [
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x08, 0x00,
        ];

        let (destination, source) = ethernet_addrs(&frame).expect("ethernet header");
        assert_eq!(destination.to_string(), "aa:bb:cc:dd:ee:ff");
        assert_eq!(source.to_string(), "00:11:22:33:44:55");
    }

    #[test]
    fn detects_broadcast_and_multicast() {
        let broadcast = MacAddress([0xff; 6]);
        let multicast = MacAddress([0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb]);
        let unicast = MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);

        assert!(broadcast.is_broadcast_or_multicast());
        assert!(multicast.is_broadcast_or_multicast());
        assert!(!unicast.is_broadcast_or_multicast());
    }

    #[test]
    fn traffic_stats_snapshot_uses_background_samples() {
        let stats = TrafficStats::new();

        stats.record_websocket_rx(100);
        stats.record_websocket_tx_batch(2, 40);
        stats.record_tap_rx(10);
        stats.sample_at(1);

        stats.record_websocket_rx(50);
        stats.record_tap_tx(25);
        stats.sample_at(6);

        let snapshot = stats.snapshot_at(6, 3);
        assert_eq!(snapshot.uptime_seconds, 6);
        assert_eq!(snapshot.recent_window_seconds, 5);
        assert_eq!(snapshot.connected_clients, 3);
        assert_eq!(snapshot.websocket_rx.frames, 2);
        assert_eq!(snapshot.websocket_rx.bytes, 150);
        assert_eq!(snapshot.websocket_rx.recent_frames, 1);
        assert_eq!(snapshot.websocket_rx.recent_bytes, 50);
        assert_eq!(snapshot.websocket_tx.frames, 2);
        assert_eq!(snapshot.websocket_tx.recent_bytes, 0);
        assert_eq!(snapshot.tap_rx.bytes, 10);
        assert_eq!(snapshot.tap_tx.recent_bytes, 25);
        assert_eq!(snapshot.current_bps, 15);
    }

    #[test]
    fn traffic_snapshot_renders_prometheus_text() {
        let stats = TrafficStats::new();

        stats.record_websocket_rx(100);
        stats.record_websocket_tx_batch(2, 40);
        stats.record_tap_rx(10);
        stats.sample_at(1);

        stats.record_websocket_rx(50);
        stats.record_tap_tx(25);
        stats.sample_at(6);

        let snapshot = stats.snapshot_at(6, 3);
        let body = format_traffic_prometheus(&snapshot);

        assert!(body.contains("# HELP websockproxy_uptime_seconds Process uptime in seconds."));
        assert!(body.contains("# TYPE websockproxy_uptime_seconds gauge"));
        assert!(body.contains("websockproxy_connected_clients 3"));
        assert!(body.contains("websockproxy_current_bytes_per_second 15"));
        assert!(body.contains(
            "websockproxy_traffic_direction_bytes_total{direction=\"websocket_rx\"} 150"
        ));
        assert!(
            body.contains("websockproxy_traffic_direction_recent_bytes{direction=\"tap_tx\"} 25")
        );
        assert!(body.contains(
            "websockproxy_traffic_direction_recent_bytes_per_second{direction=\"websocket_rx\"} 10"
        ));
        assert!(body.ends_with('\n'));
    }

    #[tokio::test]
    async fn client_broadcast_floods_to_other_clients_and_tap_without_echo() {
        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel();
        let state = AppState::new(tap_tx);

        let (client_one_tx, mut client_one_rx) = mpsc::channel(1);
        let (client_two_tx, mut client_two_rx) = mpsc::channel(1);
        state.register_client(1, &client_one_tx);
        state.register_client(2, &client_two_tx);

        let frame = ethernet_frame([0xff; 6], [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        handle_client_frame(&state, 1, frame.clone());

        assert_eq!(client_two_rx.try_recv().expect("other client frame"), frame);
        assert!(matches!(client_one_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(tap_rx.try_recv().expect("tap frame"), frame);
    }

    #[tokio::test]
    async fn client_unknown_unicast_floods_to_other_clients_and_tap() {
        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel();
        let state = AppState::new(tap_tx);

        let (client_one_tx, mut client_one_rx) = mpsc::channel(1);
        let (client_two_tx, mut client_two_rx) = mpsc::channel(1);
        state.register_client(1, &client_one_tx);
        state.register_client(2, &client_two_tx);

        let frame = ethernet_frame(
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x22],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        handle_client_frame(&state, 1, frame.clone());

        assert_eq!(client_two_rx.try_recv().expect("other client frame"), frame);
        assert!(matches!(client_one_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(tap_rx.try_recv().expect("tap frame"), frame);
    }

    #[tokio::test]
    async fn tap_unknown_unicast_floods_to_all_clients() {
        let (tap_tx, _tap_rx) = mpsc::unbounded_channel();
        let state = AppState::new(tap_tx);

        let (client_one_tx, mut client_one_rx) = mpsc::channel(1);
        let (client_two_tx, mut client_two_rx) = mpsc::channel(1);
        state.register_client(1, &client_one_tx);
        state.register_client(2, &client_two_tx);

        let frame = ethernet_frame(
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x99],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0xaa],
        );
        handle_tap_frame(&state, frame.clone());

        assert_eq!(client_one_rx.try_recv().expect("first client frame"), frame);
        assert_eq!(
            client_two_rx.try_recv().expect("second client frame"),
            frame
        );
    }

    #[tokio::test]
    async fn client_queue_drops_frames_when_full() {
        let (tap_tx, _tap_rx) = mpsc::unbounded_channel();
        let state = AppState::new(tap_tx);

        let (client_tx, mut client_rx) = mpsc::channel(1);
        state.register_client(1, &client_tx);

        let client_mac = MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert!(state.learn_mac(1, client_mac));

        let first_frame = ethernet_frame_with_payload(
            client_mac.0,
            [0x02, 0x00, 0x00, 0x00, 0x00, 0xaa],
            &[0x01],
        );
        let second_frame = ethernet_frame_with_payload(
            client_mac.0,
            [0x02, 0x00, 0x00, 0x00, 0x00, 0xaa],
            &[0x02],
        );

        handle_tap_frame(&state, first_frame.clone());
        handle_tap_frame(&state, second_frame);

        assert_eq!(client_rx.try_recv().expect("queued frame"), first_frame);
        assert!(matches!(client_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn learning_same_mac_twice_is_a_noop() {
        let (tap_tx, _tap_rx) = mpsc::unbounded_channel();
        let state = AppState::new(tap_tx);

        let (client_tx, _client_rx) = mpsc::channel(1);
        state.register_client(1, &client_tx);

        let mac = MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert!(state.learn_mac(1, mac));
        assert!(!state.learn_mac(1, mac));

        let (client_id, _sender) = state.sender_for_mac(mac).expect("mac mapping");
        assert_eq!(client_id, 1);
    }

    fn ethernet_frame(destination: [u8; 6], source: [u8; 6]) -> Bytes {
        ethernet_frame_with_payload(destination, source, &[0x08, 0x00])
    }

    fn ethernet_frame_with_payload(destination: [u8; 6], source: [u8; 6], payload: &[u8]) -> Bytes {
        let mut frame = Vec::with_capacity(14);
        frame.extend_from_slice(&destination);
        frame.extend_from_slice(&source);
        frame.extend_from_slice(payload);
        Bytes::from(frame)
    }
}
