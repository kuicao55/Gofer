#!/usr/bin/env bash
# Start/stop Gofer. Usage: ./start-gofer.sh [start|stop|restart|status|logs] [--build]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${GOFER_BIN:-$ROOT/target/release/gofer}"
PID_FILE="${GOFER_PID_FILE:-$HOME/.local/state/gofer/gofer.pid}"
LOG_FILE="${GOFER_LOG_FILE:-$HOME/.local/state/gofer/gofer.log}"
BIND="${GOFER_BIND:-127.0.0.1:8790}"
DB="${GOFER_DB:-$HOME/.local/state/gofer/gofer.db}"
mkdir -p "$(dirname "$PID_FILE")" "$(dirname "$LOG_FILE")" "$(dirname "$DB")"
cmd="${1:-start}"; shift || true
build=0; for a in "$@"; do [ "$a" = "--build" ] && build=1; done
if [ "$build" = 1 ] || [ ! -x "$BIN" ]; then
  echo "==> cargo build --release --bin gofer"
  (cd "$ROOT" && cargo build --release --bin gofer)
fi
pid=$(cat "$PID_FILE" 2>/dev/null || true)
alive() { [ -n "${1:-}" ] && kill -0 "$1" 2>/dev/null; }
case "$cmd" in
  start)
    if alive "$pid"; then echo "gofer already running pid=$pid"; exit 0; fi
    echo "==> starting gofer on $BIND"
    nohup env GOFER_DB="$DB" "$BIN" serve --bind "$BIND" >>"$LOG_FILE" 2>&1 &
    echo $! > "$PID_FILE"
    sleep 0.6
    curl -fsS --max-time 2 "http://127.0.0.1:${BIND##*:}/health" >/dev/null && echo "✓ http://$BIND" || { echo "✗ failed"; tail -20 "$LOG_FILE"; exit 1; }
    ;;
  stop)
    if alive "$pid"; then kill -TERM "$pid"; echo "stopped $pid"; else echo "not running"; fi
    rm -f "$PID_FILE"
    ;;
  restart) "$0" stop; "$0" start ;;
  status) if alive "$pid"; then echo "running pid=$pid"; else echo "not running"; fi; curl -s "http://127.0.0.1:${BIND##*:}/health" || true; echo ;;
  logs) tail -f "$LOG_FILE" ;;
  *) echo "usage: $0 [start|stop|restart|status|logs] [--build]"; exit 1 ;;
esac
