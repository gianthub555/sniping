#!/usr/bin/env bash
# DRY_RUN phase switch: tools/dryrun_phase.sh <HOT_SHADOW true|false> <HOT_DRY_LANES true|false>
# Sets the two flags in the bot's .env (lane copies every 20 s), restarts the service and turns
# paper trading back on. Refuses unless DRY_RUN=true, and keeps HOT_LIVE=false: nothing real is
# sent. The 4-hour DRY_RUN runs it at the start, at +2 h (production setting) and at +4 h (copies
# off) through systemd-run timers.
set -euo pipefail
cd /home/ubuntu/Documents/bigboss
grep -qE '^DRY_RUN=true$' .env || { echo "DRY_RUN is not true: refusing"; exit 1; }
setkv() { if grep -qE "^$1=" .env; then sed -i -E "s|^$1=.*|$1=$2|" .env; else echo "$1=$2" >> .env; fi; }
setkv HOT_SHADOW "$1"
setkv HOT_DRY_LANES "$2"
setkv HOT_DRY_SAMPLE_MS 20000
setkv HOT_LIVE false
echo "===== phase $(date -u '+%Y-%m-%dT%H:%M:%SZ'): HOT_SHADOW=$1 HOT_DRY_LANES=$2 =====" >> logs/bot.log
sudo -n systemctl restart bigboss.service
v() { { grep -E "^$1=" .env || true; } | tail -n1 | cut -d= -f2- | tr -d "\"' \r"; }
for i in $(seq 1 60); do
    if curl -s -m 2 -o /dev/null -u "$(v DASHBOARD_USER | sed 's/^$/admin/'):$(v DASHBOARD_PASSWORD)" -X POST -H 'Content-Type: application/json' \
        -d '{"enabled": true}' "http://$(v DASHBOARD_ADDR | sed 's/^$/127.0.0.1:8787/')/api/trading" -w '%{http_code}' | grep -q 200; then
        echo "paper trading on"
        exit 0
    fi
    sleep 1
done
echo "could not turn paper trading on"; exit 1
