#!/usr/bin/env bash
# mdns-scenario.sh — launch/tear down throwaway conductors for the mdns
# LAN-discovery scenarios.
#
#   Scenario A (default `up`, 2 nodes): LAN-only — bootstrap unreachable,
#     mdns on everywhere. Proves mdns discovery + dial + hello alone.
#       $0 up && $0 install && sleep 60 && $0 verify && $0 down
#   Scenario A′ (same, plus a dead relay): neither node can reach a server
#     of any kind, so the mdns direct addresses are the only working path:
#       MDNS_SCEN_RELAY_1=https://127.0.0.1:9/relay \
#       MDNS_SCEN_RELAY_2=https://127.0.0.2:9/relay $0 up
#     The hello base makes this reachable: a node derives its announced peer
#     URL from the configured relay without needing the relay to be up, and
#     preflight admits a peer that names a different relay.
#   Scenario C (`scenc-up` / `scenc-verify`, 3 nodes + local bootstrap-srv):
#     bridge topology —
#       node1: bootstrap+relay = local srv, mdns OFF, lanDiscovery OFF (WAN-style)
#       node2: bootstrap+relay unreachable, mdns ON,  lanDiscovery ON  (offline LAN)
#       node3: bootstrap+relay = local srv, mdns ON,  lanDiscovery ON  (bridge)
#     node1 and node2 share NO discovery path; the claim is that gossip
#     converges on all three via node3 anyway.
#   (There is no Scenario B: it was the dead-relay variant, which folded
#   into A′ once A′ stopped needing a reachable relay string.)
#
# SAFETY CONTRACT: this script must NEVER kill by binary name.
# `pkill -x holochain` / `killall holochain` match EVERY conductor on the
# host — including the Acorn/Moss dev sandbox conductor (also a binary named
# `holochain`). That mistake killed a live Acorn session on 2026-07-06
# (BrokenPipe panic in hc-spin's zome_call_signer) and lost uncommitted tree
# state. Teardown here is strictly:
#   1. the exact PIDs this script recorded at launch, and
#   2. as a stale-run fallback, PIDs whose /proc cmdline literally contains
#      this script's own conductor invocation ("$BIN --piped -c $ROOT/"),
#      verified per-PID before kill. A bare "$ROOT/" substring is NOT enough:
#      the shell that invoked us usually carries $ROOT in its own arguments.
set -euo pipefail

# Everything is located from the script, so a different checkout needs no
# edits; each path stays overridable by its own env var.
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd -- "$SCRIPT_DIR/.." && pwd)
SIBLINGS=$(dirname -- "$REPO")

ROOT=${MDNS_SCEN_ROOT:-/tmp/mdns-scenA}
BIN=${MDNS_SCEN_HOLOCHAIN:-$REPO/target-local/release/holochain}
HC=${MDNS_SCEN_HC:-$REPO/target-local/release/hc}
SRV_BIN=${MDNS_SCEN_SRV:-$SIBLINGS/kitsune2-lrl/target/release/kitsune2-bootstrap-srv}
# The group happ MUST be the one moss.config.json pins (`yarn fetch:group-happ`
# in the moss checkout downloads it to this path and checks its sha256). A happ
# whose wasm was built against an older HDI traps in its validation callback
# under this conductor, so the two agents warrant and block each other over
# their genesis ops: the peer store then drops the blocked agent and the
# scenario looks like a discovery failure when the network layer was fine.
HAPP=${MDNS_SCEN_HAPP:-$SIBLINGS/moss/resources/default-apps/group.happ}
PASS=${MDNS_SCEN_PASSPHRASE:-scentest}
NODES=${MDNS_SCEN_NODES:-2}
# node N gets admin port ${ADMIN_BASE}N (5551, 5552, ...)
ADMIN_BASE=${MDNS_SCEN_ADMIN_BASE:-555}
SRV_PORT=${MDNS_SCEN_SRV_PORT:-38800}
RUST_LOG_DEFAULT="info,kitsune2_bootstrap_mdns=debug,kitsune2_core::factories::core_hello=debug,kitsune2_transport_iroh=debug"

usage() {
  echo "usage: $0 {up|down|status|logs [n]|verify|srv-up|srv-down|install [seed]|scenc-up|scenc-verify|scenc-down}"
  exit 1
}

require_path() { # $1 = path, $2 = test flag, $3 = env var, $4 = what it is
  [ "$2" "$1" ] || {
    echo "$4 not found: $1"
    echo "  set $3=<path> to point somewhere else"
    exit 1
  }
}

# $ROOT is both rm -rf'd and used to find processes to kill, so refuse the
# values where either would reach beyond this scenario.
guard_root() {
  case "$ROOT" in
    "" | / | /tmp | /tmp/)
      echo "refusing to run with MDNS_SCEN_ROOT=$ROOT"
      exit 1
      ;;
  esac
  case "$REPO/" in
    "$ROOT"/*)
      echo "refusing to run with MDNS_SCEN_ROOT=$ROOT: it contains the repo at $REPO"
      exit 1
      ;;
  esac
}

# Per-node knobs (defaults: mdns on, lanDiscovery on, bootstrap unreachable):
#   MDNS_SCEN_MDNS_<n>=0|1  MDNS_SCEN_LAN_<n>=0|1
#   MDNS_SCEN_BOOTSTRAP_<n>=<url>  MDNS_SCEN_RELAY_<n>=<url>
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

# The exact conductor command line this script launches. Teardown only ever
# kills a process whose /proc cmdline contains this literally.
node_cmdline() { echo "$BIN --piped -c $ROOT/"; }

# Stop one PID and wait for it, escalating if it will not go. Callers prove
# ownership BEFORE calling this.
stop_pid() { # $1 = pid, $2 = what it is
  local pid=$1 what=$2
  [ -d "/proc/$pid" ] || return 0
  echo "  stopping $what pid $pid"
  kill "$pid" 2>/dev/null || true
  for _ in $(seq 1 20); do [ -d "/proc/$pid" ] || return 0; sleep 0.25; done
  echo "  pid $pid did not exit, SIGKILL"
  kill -9 "$pid" 2>/dev/null || true
}

# Kill one PID, but only after proving it is ours: its cmdline must contain
# the fixed string $2.
kill_verified() { # $1 = pid, $2 = required cmdline substring
  local pid=$1 want=$2
  [ -d "/proc/$pid" ] || return 0
  if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF -- "$want"; then
    stop_pid "$pid" "scenario process"
  else
    echo "  REFUSING to kill pid $pid — cmdline is not '$want' (not ours)"
  fi
}

srv_up() {
  require_path "$SRV_BIN" -x MDNS_SCEN_SRV "bootstrap-srv binary"
  mkdir -p "$ROOT/srv"
  # Launched with cwd=$ROOT/srv because its command line carries no $ROOT
  # path, so teardown proves ownership by cwd instead.
  ( cd "$ROOT/srv" && nohup "$SRV_BIN" --listen "127.0.0.1:$SRV_PORT" \
      > "$ROOT/srv/srv.log" 2>&1 & echo $! > "$ROOT/srv/srv.pid" )
  for _ in $(seq 1 20); do
    grep -qiE "listening|#kitsune2_bootstrap_srv#listening" "$ROOT/srv/srv.log" 2>/dev/null && break
    sleep 0.5
  done
  echo "bootstrap-srv: pid $(cat "$ROOT/srv/srv.pid"), http://127.0.0.1:$SRV_PORT/"
  tail -n 3 "$ROOT/srv/srv.log"
}

# The srv is launched with cwd=$ROOT/srv, and that cwd — not the pidfile — is
# what proves ownership: the pid the shell records is the process it
# backgrounded, which is not always the server itself, so killing only that
# pid can leave a live bootstrap+relay behind. Everything running under this
# scenario's srv directory is stopped, and nothing else can be.
srv_down() {
  local pf="$ROOT/srv/srv.pid" pid
  for pid in $(ls /proc 2>/dev/null | grep -E '^[0-9]+$' || true); do
    [ "$(readlink "/proc/$pid/cwd" 2>/dev/null)" = "$ROOT/srv" ] || continue
    stop_pid "$pid" "bootstrap-srv"
  done
  rm -f "$pf"
}

scen_down() {
  guard_root
  echo "== stopping scenario processes (PID-file first, cmdline-verified) =="
  local want pid
  want=$(node_cmdline)
  for pf in "$ROOT"/node*/holochain.pid; do
    [ -f "$pf" ] || continue
    kill_verified "$(cat "$pf")" "$want"
    rm -f "$pf"
  done
  srv_down
  # Stale-run fallback for a previous run whose pidfiles are gone. $ROOT/
  # only narrows the candidates; the fixed launch-command match is what
  # decides, so the invoking shell and every other conductor are skipped
  # silently rather than reported as refusals.
  for pid in $(pgrep -f -- "$ROOT/" 2>/dev/null || true); do
    [ -r "/proc/$pid/cmdline" ] || continue
    tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF -- "$want" || continue
    kill_verified "$pid" "$want"
  done
  echo "== done. other conductors on this host were left untouched =="
}

scen_up() {
  guard_root
  require_path "$BIN" -x MDNS_SCEN_HOLOCHAIN "holochain binary"
  scen_down                      # idempotent, safe: only touches $ROOT processes
  rm -rf "$ROOT"
  if [ "${MDNS_SCEN_SRV_UP:-0}" = 1 ]; then srv_up; fi
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
  require_path "$HAPP" -f MDNS_SCEN_HAPP "group happ"
  require_path "$HC" -x MDNS_SCEN_HC "hc binary"
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

# The chain under test is
#   mdns record resolved → direct dial → iroh preflight → hello grant,
# so each link gets its own count from one node's log.
#
# The conductor writes its log with ANSI styling, which lands *between* a
# tracing field name and its value, so any per-field match has to run on a
# de-styled copy of the line — stripped once per node here, not once per grep.
node_counts() { # $1 = node number -> "<dials> <direct> <grants>"
  local n=$1 log stripped dials direct grants
  log="$ROOT/node$n/conductor.log"
  [ -f "$log" ] || return 1
  stripped=$(mktemp)
  sed -e 's/\x1b\[[0-9;]*m//g' "$log" > "$stripped"
  # "dialling url=" is the dial itself. The sibling line "dialling once we
  # have a url" records a peer parked until its url is known, which is not
  # one, so the match has to include the field name.
  dials=$(grep -cF 'mdns: discovered peer, dialling url=' "$stripped" || true)
  # Only the side that wins the dial race logs this; the peer accepts the
  # same connection inbound and logs nothing here.
  direct=$(grep -F 'Connection established' "$stripped" | grep -cF 'direct=true' || true)
  grants=$(grep -cF 'granting access' "$stripped" || true)
  rm -f "$stripped"
  echo "${dials:-0} ${direct:-0} ${grants:-0}"
}

# The distinct transport urls one node's peer store holds. list-agents reports
# remote agents learned space-wide with agent_pub_key=null, but the url is
# always set, so urls are what can be counted.
peer_urls() { # $1 = node number
  local n=$1
  "$HC" client call --port "${ADMIN_BASE}${n}" list-agents 2>&1 \
    | grep -oE '"url":"[^"]*"' | sed 's/^"url":"//;s/"$//' | sort -u || true
}

# Scenario A/A′ assertions.
#
# Caveat on the peer urls: the peer store also reflects app-level blocking.
# If the installed happ's validation rejects the other agent's genesis ops
# (which it will for any happ whose wasm predates this conductor's HDI, and
# for the Moss group happ joined outside Moss's own group-creation flow),
# the warranted agent is blocked and drops out of the peer store even though
# the mdns/transport/hello chain worked. Read the three log counts as the
# discovery result and grep for "will be blocked" before blaming the network.
scen_verify() {
  local n counts dials direct grants
  for n in $(seq 1 "$NODES"); do
    counts=$(node_counts "$n" || true)
    if [ -z "$counts" ]; then
      echo "-- node$n: no log at $ROOT/node$n/conductor.log"
      continue
    fi
    read -r dials direct grants <<< "$counts"
    echo "-- node$n (${ADMIN_BASE}${n}):"
    echo "   mdns dials triggered            : $dials"
    echo "   connections established direct  : $direct"
    echo "   hello access grants             : $grants"
    echo "   list-agents distinct peer urls  :"
    peer_urls "$n" | sed 's/^/     /'
  done
}

# ---------- Scenario C: bridge topology ----------
scenc_env() {
  export MDNS_SCEN_ROOT=${MDNS_SCEN_ROOT_C:-/tmp/mdns-scenC}
  ROOT=$MDNS_SCEN_ROOT
  export MDNS_SCEN_NODES=3; NODES=3
  export MDNS_SCEN_SRV_UP=1
  local srv="http://127.0.0.1:$SRV_PORT"
  local dead="http://127.0.0.1:1"
  # node1: WAN-style — bootstrap discovery, no mdns, no LAN discovery
  export MDNS_SCEN_MDNS_1=0 MDNS_SCEN_LAN_1=0 MDNS_SCEN_BOOTSTRAP_1="$srv" MDNS_SCEN_RELAY_1="$srv"
  # node2: offline-LAN — no server of any kind reachable, mdns+LAN only.
  # Preflight admits a peer announcing a different relay, so node2 needs no
  # relay string in common with the others.
  export MDNS_SCEN_MDNS_2=1 MDNS_SCEN_LAN_2=1 MDNS_SCEN_BOOTSTRAP_2="$dead" MDNS_SCEN_RELAY_2="$dead"
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
  local n counts dials direct grants
  # node3 is the only node with a path to both others, so it is the only one
  # expected to hold all three endpoints; node1 and node2 hold themselves and
  # node3. Convergence is asserted by the op counts below, not by these.
  echo "== peer endpoints each node holds (expect 3 on the bridge, 2 elsewhere) =="
  # Shortened to the peer id, the last path segment: node2 is on a different
  # relay, so a host/port prefix would not distinguish these.
  for n in 1 2 3; do
    echo "-- node$n (${ADMIN_BASE}${n}):"
    peer_urls "$n" | sed 's|.*/||' | cut -c1-12 | sort -u | sed 's/^/   /'
  done
  echo ""
  echo "== discovery-path assertions =="
  for n in 1 2 3; do
    counts=$(node_counts "$n" || true)
    if [ -z "$counts" ]; then
      echo "  node$n: no log"
      continue
    fi
    read -r dials direct grants <<< "$counts"
    case $n in
      1) [ "$dials" = 0 ] && echo "  node1: 0 mdns dials (mdns off) ✓" \
                          || echo "  node1: $dials mdns dials — UNEXPECTED, mdns should be off ✗" ;;
      # Only the node that wins a dial race logs the dial, so between node2
      # and node3 one of the two counts is expected to be 0; their grants are
      # what proves the LAN pair came up.
      *) echo "  node$n: $dials mdns dials, $direct direct connections, $grants hello grants (grants expected >0)" ;;
    esac
  done
  # node2's bootstrap and relay both refuse connections, which surfaces as
  # the OS error rather than any line naming the bootstrap module.
  local c
  c=$(sed -e 's/\x1b\[[0-9;]*m//g' "$ROOT/node2/conductor.log" 2>/dev/null \
      | grep -cF "Connection refused" || true)
  echo "  node2: ${c:-0} refused connections (expected >0 — no server reachable)"
  echo ""
  echo "== op sync: integrated counts per space (expect 3 x own-published on every node) =="
  # Convergence = each node integrates its own genesis ops PLUS both other
  # agents'. node2 gets node1's ops purely via node3 (no shared discovery
  # path), which is the transitive-sync claim under test.
  local a dna d integ pub
  for n in 1 2 3; do
    a=$("$HC" client call --port "${ADMIN_BASE}${n}" list-cells 2>/dev/null \
        | grep -oE 'uhCAk[A-Za-z0-9_-]{40,}' | sort -u | head -1 || true)
    echo "-- node$n ($a):"
    for dna in $("$HC" client call --port "${ADMIN_BASE}${n}" list-dnas 2>/dev/null \
        | grep -oE 'uhC0k[A-Za-z0-9_-]{40,}' | sort -u || true); do
      d=$("$HC" client call --port "${ADMIN_BASE}${n}" dump-state "$dna" "$a" 2>/dev/null || true)
      integ=$(echo "$d" | grep -oE '"integrated":[0-9]+' | head -1 | cut -d: -f2 || true)
      pub=$(echo "$d" | grep -oE '"published_ops_count":[0-9]+' | head -1 | cut -d: -f2 || true)
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
