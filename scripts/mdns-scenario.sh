#!/usr/bin/env bash
# mdns-scenario.sh — launch/tear down throwaway conductors for the mdns
# LAN-discovery scenarios.
#
#   Scenario A (default `up`, 2 nodes): LAN-only — bootstrap unreachable,
#     mdns on everywhere. Proves mdns discovery + dial + hello alone.
#       $0 up && $0 install && sleep 60 && $0 verify && $0 down
#   Scenario A′ (same, plus a dead relay): both nodes point at the SAME
#     unreachable relay string, so neither can reach a server of any kind:
#       MDNS_SCEN_RELAY_1=https://127.0.0.1:9/relay \
#       MDNS_SCEN_RELAY_2=https://127.0.0.1:9/relay $0 up
#     The hello base makes this reachable: a node derives its announced peer
#     URL from the configured relay without needing the relay to be up, and
#     preflight lets peers that name the same relay through, so the mdns
#     direct addresses are the only working path.
#   Scenario C (`scenc-up` / `scenc-verify`, 3 nodes + local bootstrap-srv):
#     bridge topology —
#       node1: bootstrap+relay = local srv, mdns OFF, lanDiscovery OFF (WAN-style)
#       node2: bootstrap/relay unreachable, mdns ON,  lanDiscovery ON  (offline LAN)
#       node3: bootstrap+relay = local srv, mdns ON,  lanDiscovery ON  (bridge)
#     node1 and node2 share NO discovery path; the claim is that gossip
#     converges on all three via node3 anyway.
#
# SAFETY CONTRACT: this script must NEVER kill by binary name.
# `pkill -x holochain` / `killall holochain` match EVERY conductor on the
# host — including the Acorn/Moss dev sandbox conductor (also a binary named
# `holochain`). That mistake killed a live Acorn session on 2026-07-06
# (BrokenPipe panic in hc-spin's zome_call_signer) and lost uncommitted tree
# state. Teardown here is strictly:
#   1. the exact PIDs this script recorded at launch, and
#   2. as a stale-run fallback, PIDs whose /proc cmdline contains this
#      scenario's unique data dir ($ROOT) — verified per-PID before kill.
set -euo pipefail

LRL=/home/eric/code/metacurrency/holochain
ROOT=${MDNS_SCEN_ROOT:-/tmp/mdns-scenA}
BIN=${MDNS_SCEN_HOLOCHAIN:-$LRL/holochain-lrl/target-local/release/holochain}
HC=${MDNS_SCEN_HC:-$LRL/holochain-lrl/target-local/release/hc}
SRV_BIN=${MDNS_SCEN_SRV:-$LRL/kitsune2-lrl/target/release/kitsune2-bootstrap-srv}
# The group happ MUST be the one moss.config.json pins (`yarn fetch:group-happ`
# in the moss checkout downloads it to this path and checks its sha256). A happ
# whose wasm was built against an older HDI traps in its validation callback
# under this conductor, so the two agents warrant and block each other over
# their genesis ops: the peer store then drops the blocked agent and the
# scenario looks like a discovery failure when the network layer was fine.
HAPP=${MDNS_SCEN_HAPP:-$LRL/moss/resources/default-apps/group.happ}
PASS=${MDNS_SCEN_PASSPHRASE:-scentest}
NODES=${MDNS_SCEN_NODES:-2}
ADMIN_BASE=555     # node N gets admin port ${ADMIN_BASE}N (5551, 5552, ...)
SRV_PORT=${MDNS_SCEN_SRV_PORT:-38800}
RUST_LOG_DEFAULT="info,kitsune2_bootstrap_mdns=debug,kitsune2_core::factories::core_hello=debug,kitsune2_transport_iroh=debug"

usage() {
  echo "usage: $0 {up|down|status|logs [n]|verify|srv-up|srv-down|install [seed]|scenc-up|scenc-verify|scenc-down}"
  exit 1
}

# Per-node knobs (defaults: mdns on, lanDiscovery on, bootstrap unreachable):
#   MDNS_SCEN_MDNS_<n>=0|1  MDNS_SCEN_LAN_<n>=0|1
#   MDNS_SCEN_BOOTSTRAP_<n>=<url>  MDNS_SCEN_RELAY_<n>=<url>
# NOTE: transport_iroh preflight REFUSES peers on a different relay URL
# (lib.rs "Peer is on unknown relay"), so all nodes that should be able to
# connect must share the same relay_url STRING — including when that string
# names a relay nobody can reach (Scenario A′).
#
# mdns is configured through `network.advanced.mdnsBootstrap`, not a typed
# NetworkConfig field: NetworkConfig is deny_unknown_fields, so a typed key
# would make this config unreadable by a stock binary, whereas an unknown
# kitsune2 module key under `advanced` is simply ignored there.
write_config() { # $1 = node number
  local n=$1 v
  v="MDNS_SCEN_MDNS_${n}";      local mdns="${!v:-1}"
  v="MDNS_SCEN_LAN_${n}";       local lan="${!v:-1}"
  v="MDNS_SCEN_BOOTSTRAP_${n}"; local bootstrap="${!v:-http://127.0.0.1:1}"
  v="MDNS_SCEN_RELAY_${n}";     local relay="${!v:-$bootstrap}"  # bootstrap-srv serves the iroh relay on the same port
  [ "$mdns" = 1 ] && mdns=true || mdns=false
  [ "$lan" = 1 ]  && lan=true  || lan=false
  mkdir -p "$ROOT/node$n"
  cat > "$ROOT/node$n/conductor-config.yaml" <<EOF
data_root_path: $ROOT/node$n/data
keystore:
  type: lair_server_in_proc
network:
  bootstrap_url: $bootstrap
  relay_url: $relay
  advanced:
    mdnsBootstrap:
      enabled: $mdns
    irohTransport:
      relayAllowPlainText: true
      enableLanDiscovery: $lan
admin_interfaces:
  - driver:
      type: websocket
      port: ${ADMIN_BASE}${n}
      allowed_origins: "*"
EOF
  echo "  node$n config: mdns=$mdns lanDiscovery=$lan bootstrap=$bootstrap relay=$relay"
}

# Kill one PID, but only after proving it is ours: its cmdline (or, for the
# srv, its logfile path recorded beside the pidfile) must reference $ROOT.
kill_verified() { # $1 = pid
  local pid=$1
  [ -d "/proc/$pid" ] || return 0
  if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF "$ROOT/"; then
    echo "  stopping scenario process pid $pid"
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do [ -d "/proc/$pid" ] || return 0; sleep 0.25; done
    echo "  pid $pid did not exit, SIGKILL"
    kill -9 "$pid" 2>/dev/null || true
  else
    echo "  REFUSING to kill pid $pid — cmdline does not reference $ROOT (not ours)"
  fi
}

srv_up() {
  [ -x "$SRV_BIN" ] || { echo "bootstrap-srv not found: $SRV_BIN"; exit 1; }
  mkdir -p "$ROOT/srv"
  # --listen written into cmdline includes no $ROOT path, so launch with the
  # log under $ROOT via a wrapper arg-visible cwd trick: run with cwd=$ROOT/srv
  # and verify-by-cwd is not implemented — instead pass a harmless marker arg?
  # Simplest honest approach: record pid AND verify via /proc/<pid>/cwd.
  ( cd "$ROOT/srv" && nohup "$SRV_BIN" --listen "127.0.0.1:$SRV_PORT" \
      > "$ROOT/srv/srv.log" 2>&1 & echo $! > "$ROOT/srv/srv.pid" )
  for _ in $(seq 1 20); do
    grep -qiE "listening|#kitsune2_bootstrap_srv#listening" "$ROOT/srv/srv.log" 2>/dev/null && break
    sleep 0.5
  done
  echo "bootstrap-srv: pid $(cat "$ROOT/srv/srv.pid"), http://127.0.0.1:$SRV_PORT/"
  tail -n 3 "$ROOT/srv/srv.log"
}

srv_down() {
  local pf="$ROOT/srv/srv.pid" pid
  [ -f "$pf" ] || return 0
  pid=$(cat "$pf")
  if [ -d "/proc/$pid" ]; then
    # srv cmdline has no $ROOT path; verify by its cwd instead.
    if [ "$(readlink "/proc/$pid/cwd" 2>/dev/null)" = "$ROOT/srv" ]; then
      echo "  stopping bootstrap-srv pid $pid"
      kill "$pid" 2>/dev/null || true
    else
      echo "  REFUSING to kill pid $pid — cwd is not $ROOT/srv (not ours)"
    fi
  fi
  rm -f "$pf"
}

scen_down() {
  echo "== stopping scenario processes (PID-file first, path-verified) =="
  local pid
  for pf in "$ROOT"/node*/holochain.pid; do
    [ -f "$pf" ] || continue
    pid=$(cat "$pf")
    kill_verified "$pid"
    rm -f "$pf"
  done
  srv_down
  # Stale-run fallback: anything whose FULL COMMAND LINE mentions our data
  # dir. Never matches the Acorn/Moss sandbox conductor (different paths).
  # The shell that invoked us usually carries $ROOT in its own command line
  # too, so this ancestry must be excluded or teardown kills its caller.
  local self=$$ parent=$PPID grand
  grand=$(ps -o ppid= -p "$parent" 2>/dev/null | tr -d ' ')
  for pid in $(pgrep -f -- "$ROOT/" 2>/dev/null || true); do
    case "$pid" in
      "$self"|"$parent"|"${grand:-0}") continue ;;
    esac
    kill_verified "$pid"
  done
  echo "== done. other conductors on this host were left untouched =="
}

scen_up() {
  [ -x "$BIN" ] || { echo "holochain binary not found/executable: $BIN"; exit 1; }
  scen_down                      # idempotent, safe: only touches $ROOT processes
  rm -rf "$ROOT"
  [ "${MDNS_SCEN_SRV_UP:-0}" = 1 ] && srv_up
  for n in $(seq 1 "$NODES"); do
    write_config "$n"
    ( echo -n "$PASS" | RUST_LOG="${RUST_LOG:-$RUST_LOG_DEFAULT}" \
        nohup "$BIN" --piped -c "$ROOT/node$n/conductor-config.yaml" \
        > "$ROOT/node$n/conductor.log" 2>&1 & echo $! > "$ROOT/node$n/holochain.pid" )
    echo "node$n launched: pid $(cat "$ROOT/node$n/holochain.pid"), admin ws://127.0.0.1:${ADMIN_BASE}${n}"
  done
  echo "waiting for conductors to be ready..."
  for n in $(seq 1 "$NODES"); do
    for _ in $(seq 1 60); do
      grep -q "Conductor ready" "$ROOT/node$n/conductor.log" 2>/dev/null && break
      sleep 1
    done
    grep -q "Conductor ready" "$ROOT/node$n/conductor.log" 2>/dev/null \
      && echo "node$n: Conductor ready" \
      || { echo "node$n: NOT ready after 60s — see $ROOT/node$n/conductor.log"; exit 1; }
  done
}

scen_install() { # $1 = network seed
  local seed=${1:-mdns-scen-seed}
  [ -f "$HAPP" ] || { echo "happ not found: $HAPP"; exit 1; }
  for n in $(seq 1 "$NODES"); do
    echo "== node$n (${ADMIN_BASE}${n}): install group.happ seed=$seed"
    "$HC" client call --port "${ADMIN_BASE}${n}" install-app --app-id group "$HAPP" "$seed" 2>&1 | tail -1
    "$HC" client call --port "${ADMIN_BASE}${n}" enable-app group 2>&1 | tail -1
  done
}

scen_status() {
  local pid
  [ -f "$ROOT/srv/srv.pid" ] && { pid=$(cat "$ROOT/srv/srv.pid"); \
    [ -d "/proc/$pid" ] && echo "srv: pid $pid RUNNING (port $SRV_PORT)" || echo "srv: pid $pid dead"; }
  for pf in "$ROOT"/node*/holochain.pid; do
    [ -f "$pf" ] || { echo "no scenario nodes running under $ROOT"; return 0; }
    pid=$(cat "$pf")
    if [ -d "/proc/$pid" ]; then
      echo "$(basename "$(dirname "$pf")"): pid $pid RUNNING"
    else
      echo "$(basename "$(dirname "$pf")"): pid $pid dead (stale pidfile)"
    fi
  done
}

# Scenario A/A′ assertions. The chain under test is
#   mdns record resolved → direct dial → iroh preflight → hello grant,
# so each link gets its own count from the logs, and list-agents shows
# whether the peer store ended up holding both nodes' transport URLs.
#
# Caveat on that last one: the peer store also reflects app-level blocking.
# If the installed happ's validation rejects the other agent's genesis ops
# (which it will for any happ whose wasm predates this conductor's HDI, and
# for the Moss group happ joined outside Moss's own group-creation flow),
# the warranted agent is blocked and drops out of the peer store even though
# the mdns/transport/hello chain worked. Read the three log counts as the
# discovery result and grep for "will be blocked" before blaming the network.
#
# The conductor writes its log with ANSI styling, which lands *between* a
# tracing field name and its value, so any per-field match has to run on a
# de-styled copy of the line.
plain() { sed -e 's/\x1b\[[0-9;]*m//g' "$@"; }

scen_verify() {
  local log n dials direct grants
  for n in $(seq 1 "$NODES"); do
    log="$ROOT/node$n/conductor.log"
    [ -f "$log" ] || { echo "-- node$n: no log at $log"; continue; }
    dials=$(plain "$log" | grep -c "mdns: discovered peer, dialling" || true)
    # Only the side that wins the dial race logs this; the peer accepts the
    # same connection inbound and logs nothing here.
    direct=$(plain "$log" | grep "Connection established" | grep -c 'direct=true' || true)
    grants=$(plain "$log" | grep -c "granting access" || true)
    echo "-- node$n (${ADMIN_BASE}${n}):"
    echo "   mdns dials triggered            : $dials"
    echo "   connections established direct  : $direct"
    echo "   hello access grants             : $grants"
    echo "   list-agents distinct peer urls  :"
    "$HC" client call --port "${ADMIN_BASE}${n}" list-agents 2>&1 \
      | grep -oE '"url":"[^"]*"' | sed 's/^"url":"//;s/"$//' | sort -u | sed 's/^/     /'
  done
}

# ---------- Scenario C: bridge topology ----------
scenc_env() {
  export MDNS_SCEN_ROOT=${MDNS_SCEN_ROOT_C:-/tmp/mdns-scenC}
  ROOT=$MDNS_SCEN_ROOT
  export MDNS_SCEN_NODES=3; NODES=3
  export MDNS_SCEN_SRV_UP=1
  local srv="http://127.0.0.1:$SRV_PORT"
  # node1: WAN-style — bootstrap discovery, no mdns, no LAN discovery
  export MDNS_SCEN_MDNS_1=0 MDNS_SCEN_LAN_1=0 MDNS_SCEN_BOOTSTRAP_1="$srv" MDNS_SCEN_RELAY_1="$srv"
  # node2: offline-LAN — bootstrap UNREACHABLE (no WAN discovery), mdns+LAN
  #        only. Relay must be the same STRING as the others (see preflight
  #        note above); it plays no discovery role.
  export MDNS_SCEN_MDNS_2=1 MDNS_SCEN_LAN_2=1 MDNS_SCEN_BOOTSTRAP_2="http://127.0.0.1:1" MDNS_SCEN_RELAY_2="$srv"
  # node3: bridge — both discovery paths
  export MDNS_SCEN_MDNS_3=1 MDNS_SCEN_LAN_3=1 MDNS_SCEN_BOOTSTRAP_3="$srv" MDNS_SCEN_RELAY_3="$srv"
}

scenc_up() {
  scenc_env
  scen_up
  scen_install mdns-scenc-seed
  echo ""
  echo "Scenario C up. Give gossip ~60s, then: $0 scenc-verify"
}

scenc_verify() {
  scenc_env
  echo "== peer endpoints each node holds (expect 3 distinct URLs everywhere) =="
  # count distinct transport URLs, not pub keys: list-agents reports remote
  # agents learned space-wide with agent_pub_key=null, but the url is always set.
  for n in 1 2 3; do
    echo "-- node$n (${ADMIN_BASE}${n}):"
    "$HC" client call --port "${ADMIN_BASE}${n}" list-agents 2>&1 \
      | grep -oE '"url":"[^"]*"' | sed 's/.*38800.//;s/"//' | cut -c1-12 | sort -u | sed 's/^/   /'
  done
  echo ""
  echo "== discovery-path assertions =="
  local c
  c=$(plain "$ROOT/node1/conductor.log" 2>/dev/null | grep -c "mdns: discovered peer, dialling" || true)
  [ "${c:-0}" = 0 ] && echo "  node1: 0 mdns dials (mdns off) ✓" \
                    || echo "  node1: $c mdns dials — UNEXPECTED, mdns should be off ✗"
  c=$(plain "$ROOT/node2/conductor.log" 2>/dev/null | grep -ciE "bootstrap.*(error|failed|refused)" || true)
  echo "  node2: $c bootstrap failure lines (expected >0 — bootstrap unreachable)"
  c=$(plain "$ROOT/node2/conductor.log" 2>/dev/null | grep -c "mdns: discovered peer, dialling" || true)
  echo "  node2: $c mdns dials (expected >0)"
  c=$(plain "$ROOT/node2/conductor.log" 2>/dev/null | grep "Connection established" | grep -c 'direct=true' || true)
  echo "  node2: $c direct connections established (expected >0)"
  c=$(plain "$ROOT/node2/conductor.log" 2>/dev/null | grep -c "granting access" || true)
  echo "  node2: $c hello access grants (expected >0)"
  c=$(plain "$ROOT/node3/conductor.log" 2>/dev/null | grep -c "mdns: discovered peer, dialling" || true)
  echo "  node3: $c mdns dials (expected >0)"
  echo ""
  echo "== op sync: integrated counts per space (expect 3 x own-published on every node) =="
  # Convergence = each node integrates its own genesis ops PLUS both other
  # agents'. node2 gets node1's ops purely via node3 (no shared discovery
  # path), which is the transitive-sync claim under test.
  local a dna d integ pub
  for n in 1 2 3; do
    a=$("$HC" client call --port "${ADMIN_BASE}${n}" list-cells 2>/dev/null \
        | grep -oE 'uhCAk[A-Za-z0-9_-]{40,}' | sort -u | head -1)
    echo "-- node$n ($a):"
    for dna in $("$HC" client call --port "${ADMIN_BASE}${n}" list-dnas 2>/dev/null \
        | grep -oE 'uhC0k[A-Za-z0-9_-]{40,}' | sort -u); do
      d=$("$HC" client call --port "${ADMIN_BASE}${n}" dump-state "$dna" "$a" 2>/dev/null)
      integ=$(echo "$d" | grep -oE '"integrated":[0-9]+' | head -1 | cut -d: -f2)
      pub=$(echo "$d" | grep -oE '"published_ops_count":[0-9]+' | head -1 | cut -d: -f2)
      echo "   space ${dna:0:14}…: integrated=${integ:-?} own-published=${pub:-?}"
    done
  done
}

case "${1:-}" in
  up)          scen_up ;;
  down)        scen_down ;;
  status)      scen_status ;;
  logs)        tail -n 40 "$ROOT/node${2:-1}/conductor.log" ;;
  verify)      scen_verify ;;
  srv-up)      srv_up ;;
  srv-down)    srv_down ;;
  install)     scen_install "${2:-mdns-scen-seed}" ;;
  scenc-up)    scenc_up ;;
  scenc-verify) scenc_verify ;;
  scenc-down)  scenc_env; scen_down ;;
  *)           usage ;;
esac
