#!/usr/bin/env bash
# Gives the bot's hot cores (HOT_CORES 5,6,7) and its lane core (HOT_LANE_CORE 4) to it alone
# (the owner's instruction, 8 Oct 2026). Run with sudo rights. It does not reboot: the kernel
# settings take effect at the next boot, everything else at once.
#   - system.slice, user.slice and init.scope are held to cores 0-3 (they were 0-4), so only
#     hot.slice reaches cores 4-7;
#   - kernel command line (/etc/default/grub.d/60-hot-cores.cfg, then update-grub):
#       isolcpus=managed_irq,domain,4-7  no load balancing onto 4-7, managed IRQs kept off them
#       nohz_full=4-7                    no scheduler tick while one thread runs there
#       rcu_nocbs=4-7                    RCU callbacks run on 0-3
#       irqaffinity=0-3                  every other interrupt starts on 0-3
#   - interrupts now: the network queue's (virtio0-input.0, virtio0-output.0) stay on the cores
#     they are on (2 and 3), pinned there; every other movable interrupt goes to 0-3.
# Undo: rm /etc/default/grub.d/60-hot-cores.cfg && update-grub, and set the slices back to 0-4.
set -euo pipefail

HOUSE=0-3
ISOLATED=4-7

for unit in system.slice user.slice init.scope; do
    systemctl set-property "$unit" AllowedCPUs=$HOUSE
done

cat > /etc/default/grub.d/60-hot-cores.cfg <<EOF
# The bot's hot cores (5-7) and lane core (4): deploy/cpu-isolation.sh.
GRUB_CMDLINE_LINUX_DEFAULT="\$GRUB_CMDLINE_LINUX_DEFAULT isolcpus=managed_irq,domain,$ISOLATED nohz_full=$ISOLATED rcu_nocbs=$ISOLATED irqaffinity=$HOUSE"
EOF
update-grub 2>&1 | tail -2

irq_of() { awk -v n="$1" '$NF == n { sub(":", "", $1); print $1 }' /proc/interrupts; }
pin() { echo "$2" > "/proc/irq/$1/smp_affinity_list" 2>/dev/null || echo "irq $1: affinity not movable (managed by the kernel)"; }
net_in=$(irq_of virtio0-input.0)
net_out=$(irq_of virtio0-output.0)
[ -n "$net_in" ] && pin "$net_in" "$(cat /proc/irq/$net_in/effective_affinity_list)"
[ -n "$net_out" ] && pin "$net_out" "$(cat /proc/irq/$net_out/effective_affinity_list)"
for d in /proc/irq/[0-9]*; do
    i=${d##*/}
    [ "$i" = "$net_in" ] || [ "$i" = "$net_out" ] && continue
    [ "$(cat "$d/smp_affinity_list")" = "0-7" ] && echo $HOUSE > "$d/smp_affinity_list" 2>/dev/null || true
done
echo f > /proc/irq/default_smp_affinity   # cores 0-3 (a hex mask)

for unit in system.slice user.slice init.scope hot.slice; do
    printf '%-14s %s\n' "$unit" "$(systemctl show "$unit" -p AllowedCPUs --value)"
done
grep -h GRUB_CMDLINE_LINUX_DEFAULT /etc/default/grub.d/60-hot-cores.cfg
grep -m1 -o 'linux\s.*' /boot/grub/grub.cfg | head -1
for d in /proc/irq/[0-9]*; do
    i=${d##*/}
    name=$(awk -v n="$i:" '$1 == n { print $NF }' /proc/interrupts)
    [ -n "$name" ] && printf 'irq %-3s %-20s on %s\n' "$i" "$name" "$(cat "$d/effective_affinity_list")"
done
echo "Kernel settings take effect at the next reboot (not done here)."
