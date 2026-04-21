# Holochain over Reticulum

Holochain can run peer-to-peer traffic over the
[Reticulum](https://reticulum.network/) network stack instead of
the default iroh / tx5 (WebRTC) transports. Reticulum is better
suited for mesh, LoRa, or high-latency / low-bandwidth links where
an HTTP-backed bootstrap / signalling service is impractical: peer
discovery is announce-driven, identities are stable across restarts,
and there's no central relay to depend on.

Reticulum support is **opt-in at build time** — a reticulum-built
holochain binary does not include iroh or tx5 modules (and vice
versa). Selection happens via Cargo features, not via runtime
config.

## Backends

There are two Rust Reticulum implementations, exposed as
mutually-exclusive backend features on the underlying
`kitsune2_transport_reticulum` crate. The holochain feature names
are:

| Holochain feature                | Underlying backend                                                              | Pick when                                                                                                                                                              |
| -------------------------------- | ------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `transport-reticulum`            | [LXMF-rs](https://github.com/lightningrodlabs/LXMF-rs) (`reticulum-rs-transport`) | Direct / simple topologies. Mature Resource chunking handles arbitrary-size payloads transparently.                                                                    |
| `transport-reticulum-beechat`    | [Beechat](https://github.com/lightningrodlabs/Reticulum-rs) (`reticulum`)       | Mesh / multihop topologies. Larger per-packet MDU (2048 B vs 464 B), richer routing (`PathTable`, `PathRequests`, retransmit), configurable link restart, no rusqlite. |

The two features are compile-time mutually exclusive — enabling both
produces a `compile_error!` in `kitsune2_transport_reticulum`.

## Building

From the workspace root:

```sh
# LXMF-rs backend
cargo build --release -p holochain \
  --no-default-features \
  --features transport-reticulum,wasmer_sys,sqlite-encrypted

# Beechat backend
cargo build --release -p holochain \
  --no-default-features \
  --features transport-reticulum-beechat,wasmer_sys,sqlite-encrypted
```

The `hc` CLI picks up the same features via its own `transport-reticulum` /
`transport-reticulum-beechat` flags, which chain to `holochain_cli_sandbox`.
Build `hc` separately with the matching flag if you want a single binary set
that can drive reticulum sandboxes end-to-end:

```sh
cargo build --release -p hc \
  --no-default-features \
  --features transport-reticulum-beechat,wasmer_sys
```

Pre-built Makefile targets run the full test suite under each backend:

```sh
make build-workspace-wasmer_sys-transport_reticulum
make build-workspace-wasmer_sys-transport_reticulum_beechat
make test-workspace-wasmer_sys-transport_reticulum
make test-workspace-wasmer_sys-transport_reticulum_beechat
```

### System dependency

The Beechat backend's `reticulum` crate generates code via
`tonic-build`, which requires a system `protoc`:

```sh
# Debian / Ubuntu
sudo apt install -y protobuf-compiler
```

The LXMF backend has no such requirement.

## Conductor config

Reticulum-enabled binaries read a `reticulum:` block inside the top-level
`network:` section of `conductor-config.yaml`. When present, all kitsune2
traffic flows through Reticulum and the `bootstrap_url` / `signal_url` /
`relay_url` fields are ignored (they must still be set to pass schema
validation — point them at anything).

A minimal two-node TCP loopback config looks like:

```yaml
# conductor-a.yaml
data_root_path: ./node-a/data

keystore:
  type: lair_server_in_proc

admin_interfaces:
  - driver:
      type: websocket
      port: 9001
      allowed_origins: "*"

network:
  # Required by the schema but unused under Reticulum.
  bootstrap_url: https://unused.invalid
  signal_url: wss://unused.invalid
  relay_url: https://unused.invalid

  reticulum:
    interfaces:
      - type: tcpServer
        bind: "127.0.0.1:4242"
    identityPath: ./node-a/reticulum.identity
    announceIntervalS: 10
```

The second node is symmetric but with a `tcpClient` interface
pointing at the first:

```yaml
  reticulum:
    interfaces:
      - type: tcpClient
        target: "127.0.0.1:4242"
    identityPath: ./node-b/reticulum.identity
    announceIntervalS: 10
```

### Reticulum config fields

See
[`ReticulumTransportConfig`](../../kitsune2-lrl/crates/transport_reticulum/src/config.rs)
for the authoritative schema. Summary:

| YAML key             | Default       | Notes                                                                                                                                 |
| -------------------- | ------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `interfaces`         | *required*    | One or more of `tcpClient` / `tcpServer` / `udp`. At least one must be specified.                                                     |
| `identityPath`       | `None`        | Path to persist the Reticulum identity. Strongly recommended — without it, the announce destination (and URL) changes every restart. |
| `maxFrameBytes`      | `1 MiB`       | Cap for `send_resource()`. LXMF backend only.                                                                                         |
| `connectTimeoutS`    | `30`          | Link-establishment timeout (Reticulum 1-RTT + kitsune2 preflight).                                                                    |
| `announceIntervalS`  | `300`         | Per-space re-announce cadence. Lower values converge faster in tests.                                                                 |
| `linkIdleTimeoutS`   | `600`         | LXMF only; Beechat ignores this and uses compile-time timers.                                                                         |
| `beechat.*`          | all unset     | Beechat-only knobs: `retransmit`, `broadcast`, `rerouteEager`, `restartOutlinks`, `announceForever`. Silently ignored under LXMF.     |

### Interface variants

```yaml
# Connect out to a remote Reticulum node
- type: tcpClient
  target: "1.2.3.4:4242"

# Listen for incoming Reticulum connections
- type: tcpServer
  bind: "0.0.0.0:4242"

# UDP for LAN discovery / mesh
- type: udp
  bind: "0.0.0.0:0"
  group: "ff02::1"   # optional multicast group
```

## Running

Launch the conductor directly with your YAML:

```sh
./target/release/holochain --config-path ./conductor-a.yaml
```

For a sandboxed dev flow with `hc sandbox`, there's no dedicated
`reticulum` network subcommand — generate a sandbox with any
network type, then edit the generated `conductor-config.yaml` to
replace the `network:` block with a `reticulum:`-containing block
before running. The `hc sandbox` CLI merely forwards the build
feature so the spawned holochain binary can consume the YAML; the
YAML itself is the source of truth for transport selection.

## Identity persistence

Setting `identityPath` is strongly recommended. Without it, every
restart generates a fresh Reticulum identity, which means the
`ret://` URL changes and peers treat the restarted node as a new
peer until they pick up the next announce. With `identityPath`, the
URL is stable and gossip resumes cleanly after restart.

## Feature propagation

For downstream crates or applications consuming holochain as a
library, the `transport-reticulum` / `transport-reticulum-beechat`
features chain through:

```
holochain_client
  └── holochain
        ├── holochain_p2p
        │     └── kitsune2 / kitsune2_transport_reticulum
        ├── holochain_cascade
        └── holochain_conductor_api  (kitsune2_transport_reticulum dep)
```

Internally, an umbrella feature `transport-reticulum-any` is set by
either backend feature. Source-level `#[cfg]` guards that only care
"some reticulum backend is compiled in" key off the umbrella; a
handful of LXMF-specific test harnesses key off `transport-reticulum`
directly.
