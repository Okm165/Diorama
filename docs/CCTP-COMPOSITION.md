# Composing Diorama: mirrors, venues and CCTP bridges

Diorama is one spine, many instances. An instance is one process with one
profile:

| Profile | Process | Parent it follows | State |
|---|---|---|---|
| `mainnet` | `diorama-mainnet-engine` | an EVM chain with EIP-7928 BALs (Ethereum today) | cursor, journal, shadow registry |
| `venue` | `diorama-venue-engine` + `diorama-venue-sim` | a venue chain and its public market API (Hyperliquid today) | cursor, shadow registry, simulator state |
| `cctp` | `diorama-cctp-bridge` | any number of CCTP domains named in its own config | watcher cursors, message ledger |

Composition happens **across instances**, never inside one file. A deployment
with two mainnets, two venues and three bridges is seven instance directories
and seven Deployments; they share the spine crates, the endpoint-pool policy
and the manifests' shape, not a process.

## One URL list, one home

Every URL list is a named group in the instance's `[endpoints]` table:

```toml
[endpoints.parent_rpc]
http = ["${PARENT_RPC_1}", "${PARENT_RPC_2}"]
roles = ["archive", "trace"]
chain_id = 1

[endpoints.parent_ws]
ws = ["${PARENT_WS_1}", "${PARENT_WS_2}"]
roles = ["heads_ws"]

[endpoints.market_api]
http = ["${MARKET_API_URL}"]
roles = ["market_rest"]

[endpoints.market_ws]
ws = ["${MARKET_WS_URL}"]
roles = ["market_ws"]
```

A field references the group as `@parent_rpc`; the loader expands the group
once, before validation. The same group feeds the engine, the simulator, the
bridge and `export-env` — there is no second spelling. A singular field (a
write target, an admin URL) must resolve to exactly one endpoint; a group with
two entries is a config error, not an implicit "use the first".

The values are `${VAR}` references, so the deployment environment carries the
URLs, not a second literal in the TOML. A literal public endpoint is allowed
when it is genuinely global (the Hyperliquid public API in the venue's project
file is the one current example).

## The shared multiplexer

`diorama-anvil::EndpointPool` is the common spine for JSON-RPC, REST, WS and
beacon reads:

- each round starts at a random healthy endpoint and visits every healthy
  endpoint once;
- a failed endpoint is cooled for an exponentially growing window (1 s → 60 s)
  and retried in a later round;
- one jittered emergency probe is allowed while every endpoint is cooling;
- the per-call attempt budget bounds the whole rotation;
- writes never fan out: every chain has a single writer (the first endpoint),
  and CCTP's signer reads nonce/gas from that writer, not the pool.

`[endpoint_defaults]` is the one policy table. A service never constructs its
own default policy: `Instance::endpoint_policy()` is passed to every lane, and
`Instance::endpoint_specs()` restores each endpoint's roles, chain id and
weight from the group.

The websocket lane shares admission, health and cooldown with the executor but
owns a long-lived session loop; `rounds`/`max_attempts` bound a request, not a
session. A session that never delivers data, or goes idle, is a failed
endpoint and rotates.

## A worked topology: 2 mainnets, 2 venues, 3 bridges

| Instance | Profile | Namespace | Chain | CCTP domain |
|---|---|---|---|---|
| `mainnet-eth` | `mainnet` | `mainnet-eth` | Ethereum (1) | 0 |
| `mainnet-arb` | `mainnet` | `mainnet-arb` | Arbitrum (42161) | 3 |
| `venue-hyper` | `venue` | `venue-hyper` | HyperEVM (999) | 19 |
| `venue-base` | `venue` | `venue-base` | a second venue chain (Base, 8453) | 6 |
| `bridge-eth-hyper` | `cctp` | `bridge-eth-hyper` | Ethereum + HyperEVM | 0 ↔ 19 |
| `bridge-arb-hyper` | `cctp` | `bridge-arb-hyper` | Arbitrum + HyperEVM | 3 ↔ 19 |
| `bridge-eth-base` | `cctp` | `bridge-eth-base` | Ethereum + Base | 0 → 6 (one-way) |

Each bridge is its own directory, namespace, Secret, PVC and NodePorts. A
one-way bridge is a lane list with one entry; the control API refuses the
reverse pair because no watcher serves it.

```toml
# config/instances/bridge-arb-hyper/instance.toml
[instance]
name = "bridge-arb-hyper"
namespace = "bridge-arb-hyper"
profile = "cctp"
project = "daloc"            # optional; omit for a neutral bridge

[services.cctp_bridge]
state_dir = "${CCTP_STATE_DIR}"
relay_key = "${CCTP_RELAY_KEY}"
attester_seed = "${CCTP_ATTESTER_SEED}"

[services.cctp_bridge.iris]
port = 8095

[services.cctp_bridge.control]
port = 8096
```

The topology itself lives in the project specialization (gitignored; the
tracked template is `daloc.example.toml`):

```toml
[[services.cctp_bridge.chains]]
id = "arbitrum"
family = "evm"
chain_id = 42161
domain = 3
rpc_urls = "@arb_rpc"
usdc = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831"
token_messenger = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d"
message_transmitter = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64"

[[services.cctp_bridge.chains]]
id = "hyperevm"
family = "evm"
chain_id = 999
domain = 19
rpc_urls = "@hyper_rpc"
usdc = "0xb88339CB7199b77E23DB6E890353E22632Ba630f"
token_messenger = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d"
message_transmitter = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64"

[[services.cctp_bridge.lanes]]
source = "arbitrum"
destination = "hyperevm"

[[services.cctp_bridge.lanes]]
source = "hyperevm"
destination = "arbitrum"
```

## Same-domain forks

A CCTP domain is a chain-level fact, not an instance-level one. Two Diorama
venue instances that are both forks of HyperEVM are both chain 999 / domain
19, so one bridge cannot carry both as distinct chains (the config check
rejects duplicate domains). Run one bridge per fork, and remember that a burn
to domain 19 is visible to every bridge watching that source fork.

## What is deliberately not generic yet

- The `mainnet` engine follows an EVM parent with EIP-7928 BALs; the `venue`
  engine follows Hyperliquid's market API. A new parent kind needs a new
  adapter/profile, not just a config value.
- `ChainKind` is a closed enum (`ethereum`, `hyperliquid`). The CCTP chain
  `family` is a label; it does not select a decoder.
- The client-env exporter still emits DALOC's key spellings (`CHAIN_RPC_URL`,
  `CHAIN_1_*`, `HL_INFO_API_URL`, `IRIS_API_URL`). A generic project uses the
  instance's own service URLs; the DALOC block is a product interface, not the
  schema.

## Preflight and deployment

```sh
# Validate every instance, including its project specialization.
make config-check

# Validate and print one bridge's resolved chains/lanes without any RPC.
cargo run -p diorama-cctp-bridge -- check \
  --config config/instances/diorama-cctp/instance.toml

# Standalone bridge: namespace, StorageClass, Secret, overlay.
make cctp-bootstrap
make cctp-status
```

The bridge overlay does not create the relay Secret or the StorageClass
(`make cctp-bootstrap` does). A second bridge is a copy of the overlay with a
new namespace, Secret, PVC and NodePorts; the code needs no change.
