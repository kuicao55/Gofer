#!/usr/bin/env bash
# Start/stop Gofer. Usage: ./start-gofer.sh [start|stop|restart|status|logs|manifest] [--build] [--manifest]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -n "${GOFER_BIN:-}" ]; then
  BIN="$GOFER_BIN"
elif [ -x "$ROOT/gofer" ]; then
  BIN="$ROOT/gofer"
else
  BIN="$ROOT/target/release/gofer"
fi
PID_FILE="${GOFER_PID_FILE:-$HOME/.local/state/gofer/gofer.pid}"
LOG_FILE="${GOFER_LOG_FILE:-$HOME/.local/state/gofer/gofer.log}"
BIND="${GOFER_BIND:-127.0.0.1:8790}"
DB="${GOFER_DB:-$HOME/.local/state/gofer/gofer.db}"
mkdir -p "$(dirname "$PID_FILE")" "$(dirname "$LOG_FILE")" "$(dirname "$DB")"
cmd="${1:-start}"; shift || true
build=0; manifest=0
for a in "$@"; do
  [ "$a" = "--build" ] && build=1
  [ "$a" = "--manifest" ] && manifest=1
done
base_url() {
  local host="${BIND%:*}"
  host="${host#[}"; host="${host%]}"
  case "$host" in ""|"0.0.0.0"|"::") host="127.0.0.1" ;; esac
  if [[ "$host" == *:* ]]; then host="[$host]"; fi
  printf 'http://%s:%s' "$host" "${BIND##*:}"
}
MANIFEST_BASE="${GOFER_MANIFEST_BASE_URL:-$(base_url)}"
emit_manifest() {
  [ "$manifest" = 1 ] && "$BIN" manifest --base-url "$MANIFEST_BASE" || true
}
if [ "$build" = 1 ] || [ ! -x "$BIN" ]; then
  echo "==> cargo build --release --bin gofer"
  (cd "$ROOT" && cargo build --release --bin gofer)
fi
pid=$(cat "$PID_FILE" 2>/dev/null || true)
alive() { [ -n "${1:-}" ] && kill -0 "$1" 2>/dev/null; }
case "$cmd" in
  start)
    if alive "$pid"; then echo "gofer already running pid=$pid"; emit_manifest; exit 0; fi
    echo "==> starting gofer on $BIND"
    ENV_ARGS=(GOFER_DB="$DB" GOFER_PID_FILE="$PID_FILE" GOFER_LOG_FILE="$LOG_FILE")
    # Binaries embed scripts/web by default. Only pass explicit overrides.
    if [ -n "${GOFER_SCRIPTS_DIR:-}" ]; then
      ENV_ARGS+=(GOFER_SCRIPTS_DIR="$GOFER_SCRIPTS_DIR")
    fi
    if [ -n "${GOFER_WEB_DIR:-}" ]; then
      ENV_ARGS+=(GOFER_WEB_DIR="$GOFER_WEB_DIR")
    fi
    nohup env "${ENV_ARGS[@]}" "$BIN" serve --bind "$BIND" >>"$LOG_FILE" 2>&1 &
    echo $! > "$PID_FILE"
    ready=0
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
      if curl -fsS --max-time 1 "$(base_url)/health" >/dev/null 2>&1; then
        ready=1
        break
      fi
      kill -0 $! 2>/dev/null || break
      sleep 0.5
    done
    if [ "$ready" = 1 ]; then
      echo "✓ $(base_url)"
      emit_manifest
    else
      echo "✗ failed"
      tail -20 "$LOG_FILE"
      exit 1
    fi
    ;;
  stop)
    if alive "$pid"; then kill -TERM "$pid"; echo "stopped $pid"; else echo "not running"; fi
    rm -f "$PID_FILE"
    ;;
  restart) "$0" stop; "$0" start "$@" ;;
  status) if alive "$pid"; then echo "running pid=$pid"; else echo "not running"; fi; curl -s "$(base_url)/health" || true; echo ;;
  logs) tail -f "$LOG_FILE" ;;
  manifest) "$BIN" manifest --base-url "$MANIFEST_BASE" ;;
  *) echo "usage: $0 [start|stop|restart|status|logs|manifest] [--build] [--manifest]"; exit 1 ;;
esac
