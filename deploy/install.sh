#!/usr/bin/env bash
# Installs the bot's systemd unit and reserves cores 4-7 for it (C4):
#   - bigboss.service runs in hot.slice (cores 0-7); its hot threads pin to HOT_CORES (5,6,7);
#   - system.slice, user.slice and init.scope are held to cores 0-3 (cgroup cpuset, persistent;
#     core 4 is the lane core, HOT_LANE_CORE; deploy/cpu-isolation.sh isolates 4-7);
#   - the shredstream proxy and the bot are enabled, with Restart=always. pumpfeed's engine runs
#     inside the bot now (ENGINE_INPROC): pumpfeed.service is disabled, its files are kept.
# Run with sudo rights. Starts nothing; start the services with systemctl (or ./bot.sh start).
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

install -m 644 bigboss.service /etc/systemd/system/bigboss.service
install -m 644 hot.slice /etc/systemd/system/hot.slice
systemctl daemon-reload
for unit in system.slice user.slice init.scope; do
    systemctl set-property "$unit" AllowedCPUs=0-3
done
systemctl enable shredstream-proxy.service bigboss.service
systemctl disable --now pumpfeed.service 2>/dev/null || true
for unit in system.slice user.slice init.scope hot.slice; do
    printf '%-14s %s\n' "$unit" "$(systemctl show "$unit" -p AllowedCPUs --value)"
done
systemctl is-enabled shredstream-proxy.service bigboss.service
