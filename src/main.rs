use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        ConnectInfo, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
    routing::get,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    fmt,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use tun::{AbstractDevice, Layer};

type ClientId = u64;
type Frame = Vec<u8>;

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

#[derive(Clone)]
struct ClientHandle {
    tx: mpsc::UnboundedSender<Frame>,
    mac: Option<MacAddress>,
}

#[derive(Default)]
struct SwitchTable {
    clients: HashMap<ClientId, ClientHandle>,
    mac_index: HashMap<MacAddress, ClientId>,
}

struct AppState {
    next_client_id: AtomicU64,
    tap_tx: mpsc::UnboundedSender<Frame>,
    table: RwLock<SwitchTable>,
}

impl AppState {
    fn new(tap_tx: mpsc::UnboundedSender<Frame>) -> Self {
        Self {
            next_client_id: AtomicU64::new(1),
            tap_tx,
            table: RwLock::new(SwitchTable::default()),
        }
    }

    fn next_client_id(&self) -> ClientId {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn register_client(&self, client_id: ClientId, tx: mpsc::UnboundedSender<Frame>) {
        let mut table = self.table.write().await;
        table
            .clients
            .insert(client_id, ClientHandle { tx, mac: None });
    }

    async fn unregister_client(&self, client_id: ClientId) {
        let mut table = self.table.write().await;
        if let Some(client) = table.clients.remove(&client_id) {
            if let Some(mac) = client.mac {
                if table.mac_index.get(&mac) == Some(&client_id) {
                    table.mac_index.remove(&mac);
                }
            }
        }
    }

    async fn learn_mac(&self, client_id: ClientId, mac: MacAddress) -> bool {
        let mut table = self.table.write().await;

        let previous_mac = match table.clients.get_mut(&client_id) {
            Some(client) => {
                if client.mac == Some(mac) {
                    return false;
                }
                client.mac.replace(mac)
            }
            None => return false,
        };

        if let Some(old_mac) = previous_mac {
            if table.mac_index.get(&old_mac) == Some(&client_id) {
                table.mac_index.remove(&old_mac);
            }
        }

        if let Some(previous_owner) = table.mac_index.insert(mac, client_id) {
            if previous_owner != client_id {
                if let Some(previous_client) = table.clients.get_mut(&previous_owner) {
                    previous_client.mac = None;
                }
            }
        }

        true
    }

    async fn sender_for_mac(
        &self,
        mac: MacAddress,
    ) -> Option<(ClientId, mpsc::UnboundedSender<Frame>)> {
        let table = self.table.read().await;
        let client_id = *table.mac_index.get(&mac)?;
        table
            .clients
            .get(&client_id)
            .map(|client| (client_id, client.tx.clone()))
    }

    async fn all_client_senders(&self) -> Vec<mpsc::UnboundedSender<Frame>> {
        let table = self.table.read().await;
        table
            .clients
            .values()
            .map(|client| client.tx.clone())
            .collect()
    }

    async fn other_client_senders(
        &self,
        excluded_client_id: ClientId,
    ) -> Vec<mpsc::UnboundedSender<Frame>> {
        let table = self.table.read().await;
        table
            .clients
            .iter()
            .filter(|(client_id, _client)| **client_id != excluded_client_id)
            .map(|(_client_id, client)| client.tx.clone())
            .collect()
    }

    fn send_to_tap(&self, frame: Frame) -> Result<(), mpsc::error::SendError<Frame>> {
        self.tap_tx.send(frame)
    }
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

    let app = Router::new()
        .route("/", get(ws_handler))
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
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    state.register_client(client_id, client_tx).await;

    info!(client_id, peer = %peer, "client connected");

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let peer_for_writer = peer.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = client_rx.recv().await {
            if ws_sender.send(Message::Binary(frame.into())).await.is_err() {
                debug!(client_id, peer = %peer_for_writer, "client writer stopped");
                break;
            }
        }
    });

    while let Some(message) = ws_receiver.next().await {
        match message {
            Ok(Message::Binary(frame)) => {
                handle_client_frame(&state, client_id, frame.to_vec()).await;
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Text(_)) => {
                debug!(client_id, peer = %peer, "ignoring text frame");
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Err(error) => {
                warn!(client_id, peer = %peer, %error, "websocket receive error");
                break;
            }
        }
    }

    writer_task.abort();
    state.unregister_client(client_id).await;
    info!(client_id, peer = %peer, "client disconnected");
}

async fn handle_client_frame(state: &AppState, client_id: ClientId, frame: Frame) {
    let (destination, source) = match ethernet_addrs(&frame) {
        Some(addrs) => addrs,
        None => {
            warn!(client_id, "dropping short websocket frame");
            return;
        }
    };

    if state.learn_mac(client_id, source).await {
        info!(client_id, mac = %source, "learned client MAC");
    }

    if destination.is_broadcast_or_multicast() {
        let targets = state.other_client_senders(client_id).await;
        for target in targets {
            if let Err(error) = target.send(frame.clone()) {
                debug!(client_id, %error, "failed to queue broadcast frame for client");
            }
        }

        if let Err(error) = state.send_to_tap(frame) {
            warn!(client_id, %error, "failed to forward broadcast frame to TAP");
        }

        return;
    }

    if let Some((target_client_id, target)) = state.sender_for_mac(destination).await {
        if target_client_id == client_id {
            debug!(client_id, destination = %destination, "dropping frame addressed back to ingress client");
            return;
        }

        if let Err(error) = target.send(frame) {
            debug!(client_id, %error, destination = %destination, "failed to queue unicast frame for client");
        }
        return;
    }

    let targets = state.other_client_senders(client_id).await;
    for target in targets {
        if let Err(error) = target.send(frame.clone()) {
            debug!(client_id, %error, destination = %destination, "failed to queue flooded frame for client");
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
                match outgoing {
                    Some(frame) => {
                        device.send(&frame).await.context("failed to write frame to TAP")?;
                    }
                    None => {
                        warn!("tap writer channel closed");
                        return Ok(());
                    }
                }
            }
            incoming = device.recv(&mut buffer) => {
                let size = incoming.context("failed to read frame from TAP")?;
                if size == 0 {
                    continue;
                }
                handle_tap_frame(&state, buffer[..size].to_vec()).await;
            }
        }
    }
}

async fn handle_tap_frame(state: &AppState, frame: Frame) {
    let Some((destination, _source)) = ethernet_addrs(&frame) else {
        warn!("dropping short TAP frame");
        return;
    };

    if destination.is_broadcast_or_multicast() {
        let targets = state.all_client_senders().await;
        for target in targets {
            if let Err(error) = target.send(frame.clone()) {
                debug!(%error, "failed to queue TAP broadcast frame for client");
            }
        }
        return;
    }

    if let Some((_client_id, target)) = state.sender_for_mac(destination).await {
        if let Err(error) = target.send(frame) {
            debug!(
                destination = %destination,
                %error,
                "failed to queue TAP unicast frame for client"
            );
        }
        return;
    }

    let targets = state.all_client_senders().await;
    for target in targets {
        if let Err(error) = target.send(frame.clone()) {
            debug!(
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
mod tests {
    use super::{AppState, MacAddress, ethernet_addrs, handle_client_frame, handle_tap_frame};
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

    #[tokio::test]
    async fn client_broadcast_floods_to_other_clients_and_tap_without_echo() {
        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel();
        let state = AppState::new(tap_tx);

        let (client_one_tx, mut client_one_rx) = mpsc::unbounded_channel();
        let (client_two_tx, mut client_two_rx) = mpsc::unbounded_channel();
        state.register_client(1, client_one_tx).await;
        state.register_client(2, client_two_tx).await;

        let frame = ethernet_frame([0xff; 6], [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        handle_client_frame(&state, 1, frame.clone()).await;

        assert_eq!(client_two_rx.try_recv().expect("other client frame"), frame);
        assert!(matches!(client_one_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(tap_rx.try_recv().expect("tap frame"), frame);
    }

    #[tokio::test]
    async fn client_unknown_unicast_floods_to_other_clients_and_tap() {
        let (tap_tx, mut tap_rx) = mpsc::unbounded_channel();
        let state = AppState::new(tap_tx);

        let (client_one_tx, mut client_one_rx) = mpsc::unbounded_channel();
        let (client_two_tx, mut client_two_rx) = mpsc::unbounded_channel();
        state.register_client(1, client_one_tx).await;
        state.register_client(2, client_two_tx).await;

        let frame = ethernet_frame(
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x22],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        handle_client_frame(&state, 1, frame.clone()).await;

        assert_eq!(client_two_rx.try_recv().expect("other client frame"), frame);
        assert!(matches!(client_one_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(tap_rx.try_recv().expect("tap frame"), frame);
    }

    #[tokio::test]
    async fn tap_unknown_unicast_floods_to_all_clients() {
        let (tap_tx, _tap_rx) = mpsc::unbounded_channel();
        let state = AppState::new(tap_tx);

        let (client_one_tx, mut client_one_rx) = mpsc::unbounded_channel();
        let (client_two_tx, mut client_two_rx) = mpsc::unbounded_channel();
        state.register_client(1, client_one_tx).await;
        state.register_client(2, client_two_tx).await;

        let frame = ethernet_frame(
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x99],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0xaa],
        );
        handle_tap_frame(&state, frame.clone()).await;

        assert_eq!(client_one_rx.try_recv().expect("first client frame"), frame);
        assert_eq!(
            client_two_rx.try_recv().expect("second client frame"),
            frame
        );
    }

    fn ethernet_frame(destination: [u8; 6], source: [u8; 6]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(14);
        frame.extend_from_slice(&destination);
        frame.extend_from_slice(&source);
        frame.extend_from_slice(&[0x08, 0x00]);
        frame
    }
}
