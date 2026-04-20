# websocketproxy

`websockproxy-relay` is a Rust Layer 2 relay that carries raw Ethernet frames over WebSocket and bridges them with a local TAP device.

## What it does

- Accepts WebSocket connections on `/`
- Learns client source MAC addresses and forwards unicast traffic to the known destination
- Floods broadcast, multicast, and unknown unicast frames to other clients
- Bridges traffic between connected WebSocket clients and a local TAP interface
- Supports CLI flags and environment variables for runtime configuration

## Requirements

- Rust 1.85 or newer
- A system that supports TAP devices
- Sufficient privileges to create and bring up a TAP interface

## Build

With Make:

```bash
make build
make test
make release
```

Or directly with Cargo:

```bash
cargo build --release
```

`make release` creates a single binary at `dist/websockproxy-relay`.

## Run

```bash
sudo RUST_LOG=info cargo run --release -- \
  --listen-addr 0.0.0.0:80 \
  --network-mode none \
  --tap-name tap0 \
  --tap-mtu 1500
```

The WebSocket endpoint is exposed at `/`.
`--version` prints the current git tag, or the commit hash if there is no tag on `HEAD`.

GitHub Actions publishes the release binary automatically for tags that match `v*`.

## Network modes

The relay can configure the host networking for the TAP device with `--network-mode`.

- `none`: create and use the TAP device only. The host networking is left unchanged.
- `bridge`: create or reuse a Linux bridge, move the uplink IPv4 addresses/default route to it, and attach both the uplink and TAP device.
- `nat`: assign a gateway address to the TAP device, enable IPv4 forwarding, and install nftables NAT/FORWARD rules directly via netlink for the selected uplink.
- If `--uplink-if` is omitted, the relay first uses the interface from the default IPv4 route. If that is unavailable, it falls back to the only physical NIC when exactly one exists.

Examples:

```bash
# TAP only
sudo cargo run --release -- \
  --network-mode none \
  --tap-name tap0

# Bridge tap0 into the physical uplink
sudo cargo run --release -- \
  --network-mode bridge \
  --bridge-name br0 \
  --tap-name tap0

# NAT clients behind tap0 out through the physical uplink
sudo cargo run --release -- \
  --network-mode nat \
  --nat-network 10.200.0.0/24 \
  --tap-name tap0
```

## Metrics (Prometheus)

The server exposes `GET /metrics`, returning Prometheus text metrics for traffic totals and recent throughput.

Prometheus includes a built-in Web UI at port `9090` (Graph, Targets, etc.).

### Run Prometheus with Docker

1. Run the relay on the host (example):

```bash
sudo RUST_LOG=info cargo run --release -- \
  --listen-addr 0.0.0.0:80 \
  --network-mode none \
  --tap-name tap0 \
  --tap-mtu 1500
```

2. Start Prometheus (Docker Compose):

```bash
docker compose up -d
```

3. Open the Prometheus UI:

- `http://localhost:9090`

Then check `Status -> Targets` and run queries like:

- `websockproxy_current_bytes_per_second`
- `websockproxy_connected_clients`

## Configuration

| Flag | Environment variable | Default |
| --- | --- | --- |
| `--listen-addr` | `LISTEN_ADDR` | `0.0.0.0:80` |
| `--network-mode` | `NETWORK_MODE` | `none` |
| `--uplink-if` | `UPLINK_IF` | default IPv4 route interface |
| `--bridge-name` | `BRIDGE_NAME` | `br0` |
| `--nat-network` | `NAT_NETWORK` | `10.200.0.0/24` |
| `--tap-name` | `TAP_NAME` | `tap0` |
| `--tap-mtu` | `TAP_MTU` | `1500` |

## Development

```bash
cargo test
```

The server uses `tracing` for logs and respects `RUST_LOG`. When traffic is forwarded through a reverse proxy, `X-Forwarded-For` is used for peer logging when present.
