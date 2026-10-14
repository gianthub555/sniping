#!/usr/bin/env bash
# Manage the sniper bot: build, start, stop, restart, status, logs.
#
#   ./bot.sh build     compile the release binary
#   ./bot.sh start     build if needed, then run the bot in the background
#   ./bot.sh stop      stop the bot (graceful first, forced after a timeout)
#   ./bot.sh restart   rebuild, then stop and start (keeps the old bot running if the build fails)
#   ./bot.sh status    show whether the bot is running
#   ./bot.sh logs      follow the log file (Ctrl-C to leave; the bot keeps running)
#   ./bot.sh buy-size [SOL]
#                      show the buy size, or set it (applied at once if the bot is running,
#                      and saved to .env either way)
#   ./bot.sh latency [SECONDS] [--land-test N]
#                      measure the launch-to-buy latency chain (default 120 s, sends nothing;
#                      --land-test N sends N memo transactions, paying the tip for each)

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

BINARY="target/release/sniper-bot"
PID_FILE=".bot.pid"
# Installed by deploy/install.sh: then the bot runs as this systemd service (Restart=always, in
# hot.slice with cores 5-7 to itself) and start/stop/status go through systemd.
SERVICE="bigboss.service"
LOG_DIR="logs"
LOG_FILE="$LOG_DIR/bot.log"
STOP_TIMEOUT=15
STARTUP_TIMEOUT=60

if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
fi

use_systemd() {
    [ -f "/etc/systemd/system/$SERVICE" ]
}

running_pid() {
    if use_systemd; then
        local pid
        pid=$(systemctl show -p MainPID --value "$SERVICE")
        [ -n "$pid" ] && [ "$pid" != "0" ] && echo "$pid"
        return
    fi
    [ -f "$PID_FILE" ] || return 1
    local pid
    pid=$(cat "$PID_FILE")
    # Only trust the PID file if that process is really this bot (PIDs get reused).
    if [ -n "$pid" ] && [ -r "/proc/$pid/cmdline" ] && grep -qa "$BINARY" "/proc/$pid/cmdline"; then
        echo "$pid"
        return 0
    fi
    rm -f "$PID_FILE"
    return 1
}

# Value of a setting in .env (empty if unset; never fails, so `set -e` is safe).
env_value() {
    { grep -E "^$1=" .env 2>/dev/null || true; } | tail -n 1 | cut -d= -f2- | tr -d "\"' \r"
}

build() {
    echo "Building release binary..."
    cargo build --release --bin sniper-bot
}

start() {
    local pid
    if pid=$(running_pid); then
        echo "Bot is already running (PID $pid). Use ./bot.sh restart to restart it."
        return 1
    fi
    if [ ! -f .env ]; then
        echo "No .env file. Copy .env.example to .env and fill it in first."
        return 1
    fi
    # Always build: starting an old binary after the code changed runs code that is not
    # what you are looking at (cargo does nothing when the build is already current).
    build || { echo "Build failed; not starting."; return 1; }

    if [ "$(env_value DRY_RUN | tr '[:upper:]' '[:lower:]')" = "false" ]; then
        echo "WARNING: DRY_RUN=false - the bot will send real transactions with real SOL."
    else
        echo "DRY_RUN is on: transactions are simulated, nothing is sent."
    fi

    mkdir -p "$LOG_DIR"
    touch "$LOG_FILE"
    # Only look at log lines written by this start, not by earlier runs.
    local log_offset
    log_offset=$(wc -l <"$LOG_FILE")
    echo "===== start $(date -u '+%Y-%m-%dT%H:%M:%SZ') =====" >>"$LOG_FILE"
    if use_systemd; then
        sudo systemctl start "$SERVICE"
        pid=$(running_pid) || { echo "The service did not start: systemctl status $SERVICE"; return 1; }
    else
        nohup "$BINARY" >>"$LOG_FILE" 2>&1 &
        pid=$!
        echo "$pid" >"$PID_FILE"
    fi

    # Wait until the bot finishes its startup checks (RPC, landing service, gRPC) or exits.
    local waited=0
    while [ "$waited" -lt "$STARTUP_TIMEOUT" ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            rm -f "$PID_FILE"
            use_systemd && sudo systemctl stop "$SERVICE"
            echo "Bot exited during startup (service stopped, so it does not restart in a loop). Last log lines:"
            tail -n 15 "$LOG_FILE"
            return 1
        fi
        if awk -v from="$log_offset" 'NR > from && /Starting sniper bot/ { found = 1 } END { exit !found }' "$LOG_FILE"; then
            echo "Bot started (PID $pid). Logs: $LOG_FILE"
            local dashboard
            dashboard=$(env_value DASHBOARD_ADDR)
            if [ "$(env_value DASHBOARD_ENABLED | tr '[:upper:]' '[:lower:]')" != "false" ]; then
                echo "Dashboard: http://${dashboard:-127.0.0.1:8787}"
            fi
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    echo "Bot is running (PID $pid) but has not finished starting after ${STARTUP_TIMEOUT}s. Check $LOG_FILE"
}

stop() {
    local pid
    if ! pid=$(running_pid); then
        echo "Bot is not running."
        return 0
    fi
    echo "Stopping bot (PID $pid)..."
    if use_systemd; then
        # The unit stops it with SIGINT and forces it after TimeoutStopSec.
        sudo systemctl stop "$SERVICE"
        echo "===== stop $(date -u '+%Y-%m-%dT%H:%M:%SZ') =====" >>"$LOG_FILE"
        echo "Bot stopped."
        return 0
    fi
    # SIGINT is what the pipeline listens for (same as Ctrl-C).
    kill -INT "$pid" 2>/dev/null || true
    local waited=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$waited" -ge "$STOP_TIMEOUT" ]; then
            echo "Still running after ${STOP_TIMEOUT}s; forcing it to stop."
            kill -KILL "$pid" 2>/dev/null || true
            break
        fi
        sleep 1
        waited=$((waited + 1))
    done
    rm -f "$PID_FILE"
    echo "===== stop $(date -u '+%Y-%m-%dT%H:%M:%SZ') =====" >>"$LOG_FILE"
    echo "Bot stopped."
}

dashboard_url() {
    local addr
    addr=$(env_value DASHBOARD_ADDR)
    echo "http://${addr:-127.0.0.1:8787}"
}

# curl options for the dashboard: its login, when DASHBOARD_PASSWORD is set.
dashboard_auth() {
    local password
    password=$(env_value DASHBOARD_PASSWORD)
    if [ -n "$password" ]; then
        printf -- '-u\n%s:%s\n' "$(env_value DASHBOARD_USER | sed 's/^$/admin/')" "$password"
    fi
}

dashboard_enabled() {
    [ "$(env_value DASHBOARD_ENABLED | tr '[:upper:]' '[:lower:]')" != "false" ]
}

buy_size() {
    local sol="${1:-}"
    local pid=""
    pid=$(running_pid) || pid=""

    if [ -z "$sol" ]; then
        if [ -n "$pid" ] && dashboard_enabled; then
            local current
            local auth=()
            mapfile -t auth < <(dashboard_auth)
            current=$(curl -s -m 5 "${auth[@]}" "$(dashboard_url)/api/state" | grep -o '"buy_sol":[^,]*' | cut -d: -f2 || true)
            echo "Buy size: ${current:-unknown} SOL per buy (running bot)"
        else
            echo "Buy size: $(env_value BUY_SOL_AMOUNT) SOL per buy (from .env)"
        fi
        return 0
    fi

    if ! [[ "$sol" =~ ^[0-9]*\.?[0-9]+$ ]] || ! awk -v v="$sol" 'BEGIN { exit !(v > 0) }'; then
        echo "Buy size must be a positive number of SOL, e.g. ./bot.sh buy-size 0.01"
        return 1
    fi
    [[ "$sol" == .* ]] && sol="0$sol"  # JSON needs a leading zero

    if [ -n "$pid" ] && dashboard_enabled; then
        local response code auth=()
        mapfile -t auth < <(dashboard_auth)
        response=$(curl -s -m 10 -w '\n%{http_code}' "${auth[@]}" -X POST -H 'Content-Type: application/json' \
            -d "{\"sol\": $sol}" "$(dashboard_url)/api/buy-size") || {
            echo "Could not reach the running bot at $(dashboard_url)."
            return 1
        }
        code=$(tail -n 1 <<<"$response")
        response=$(sed '$d' <<<"$response")
        if [ "$code" = "200" ]; then
            echo "Buy size is now $(grep -o '"buy_sol":[^,}]*' <<<"$response" | cut -d: -f2) SOL per buy (applied to the running bot, saved to .env)."
        else
            echo "Not changed: $(grep -o '"error":"[^"]*"' <<<"$response" | cut -d'"' -f4)"
            return 1
        fi
        return 0
    fi

    if grep -qE '^BUY_SOL_AMOUNT=' .env; then
        sed -i -E "s/^BUY_SOL_AMOUNT=.*/BUY_SOL_AMOUNT=$sol/" .env
    else
        echo "BUY_SOL_AMOUNT=$sol" >>.env
    fi
    if [ -n "$pid" ]; then
        echo "Saved BUY_SOL_AMOUNT=$sol to .env. The dashboard is disabled, so restart the bot to apply it."
    else
        echo "Saved BUY_SOL_AMOUNT=$sol to .env. It applies when the bot starts."
    fi
}

status() {
    local pid
    if pid=$(running_pid); then
        local started
        started=$(ps -o lstart= -p "$pid" | sed 's/^ *//')
        echo "Bot is running (PID $pid, started $started)."
        echo "Mode: $( [ "$(env_value DRY_RUN | tr '[:upper:]' '[:lower:]')" = "false" ] && echo LIVE || echo dry run )"
    else
        echo "Bot is not running."
    fi
}

case "${1:-}" in
    build) build ;;
    start) start ;;
    stop) stop ;;
    restart)
        # Build first so a compile error never leaves you with no bot running.
        build
        stop
        start
        ;;
    status) status ;;
    buy-size) buy_size "${2:-}" ;;
    latency)
        shift
        seconds=120
        if [[ "${1:-}" =~ ^[0-9]+$ ]]; then
            seconds=$1
            shift
        fi
        echo "Building the latency tool..."
        cargo build --release --bin latency
        ./target/release/latency --seconds "$seconds" "$@"
        ;;
    logs)
        mkdir -p "$LOG_DIR"
        touch "$LOG_FILE"
        tail -n 50 -f "$LOG_FILE"
        ;;
    compare)
        # Each buy since SINCE (default: the last 24 h) replayed over the token's real trades:
        # the model next to what the bot got, live and paper separately.
        since="${2:-$(date -u -d '24 hours ago' +%Y-%m-%dT%H:%M)}"
        cargo build --release --bin rules_check >/dev/null 2>&1 || { echo "build failed"; exit 1; }
        python3 tools/fetch_tapes.py "$since" || exit 1
        # Live and paper histories are kept apart (separate logs), and reported apart.
        for log in auto_sell_logs.csv auto_sell_logs_paper.csv; do
            [ -f "$log" ] || continue
            echo "===== $log ($( [ "$log" = auto_sell_logs.csv ] && echo live || echo paper ) trades) ====="
            ./target/release/rules_check play "$LOG_DIR/tapes" "$log" "$since"
        done
        ;;
    *)
        echo "Usage: $0 {build|start|stop|restart|status|logs|compare [SINCE]|buy-size [SOL]|latency [SECONDS] [--land-test N]}"
        exit 1
        ;;
esac
