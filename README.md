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

```bash
cargo build --release
```

## Run

```bash
sudo RUST_LOG=info cargo run --release -- \
  --listen-addr 0.0.0.0:8080 \
  --tap-name tap0 \
  --tap-mtu 1500
```

The WebSocket endpoint is exposed at `/`.

## Configuration

| Flag | Environment variable | Default |
| --- | --- | --- |
| `--listen-addr` | `LISTEN_ADDR` | `0.0.0.0:80` |
| `--tap-name` | `TAP_NAME` | `tap0` |
| `--tap-mtu` | `TAP_MTU` | `1500` |

## Development

```bash
cargo test
```

The server uses `tracing` for logs and respects `RUST_LOG`. When traffic is forwarded through a reverse proxy, `X-Forwarded-For` is used for peer logging when present.
