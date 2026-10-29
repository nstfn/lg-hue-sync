#!/usr/bin/env bash
set -euo pipefail

TV_IP="${1:?Usage: ./scripts/uninstall.sh <tv-ip> [ssh-port] [--purge-config]}"
SSH_PORT="${2:-22}"
PURGE_CONFIG=false
for option in "${@:3}"; do
    case "$option" in
        --purge-config) PURGE_CONFIG=true ;;
        *) echo "Unknown option: $option" >&2; exit 2 ;;
    esac
done

SSH_OPTS=(-p "$SSH_PORT" -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5)
purge_value=false
$PURGE_CONFIG && purge_value=true

ssh "${SSH_OPTS[@]}" root@"$TV_IP" "PURGE_CONFIG=$purge_value sh -s" <<'REMOTE'
set -eu
install_dir=/var/home/root/lg-hue-sync
backup_dir="/var/home/root/lg-hue-sync-backup-$(date +%Y%m%d-%H%M%S)"

systemctl stop lg-hue-sync 2>/dev/null || true
systemctl disable lg-hue-sync 2>/dev/null || true
systemctl stop lg-hue-sync-log.timer lg-hue-sync-log.service 2>/dev/null || true
rm -f /run/systemd/system/lg-hue-sync-log.service /run/systemd/system/lg-hue-sync-log.timer
rm -f /var/lib/webosbrew/init.d/55-lg-hue-sync-log
rm -f /run/systemd/system/lg-hue-sync.service /etc/systemd/system/lg-hue-sync.service
rm -f /var/lib/webosbrew/init.d/50-lg-hue-sync

for root in /var/luna-service2 /var/luna-service2-dev; do
    rm -f "$root/manifests.d/org.webosbrew.lg-hue-sync.json"
    rm -f "$root/roles.d/org.webosbrew.lg-hue-sync.json"
    rm -f "$root/client-permissions.d/org.webosbrew.lg-hue-sync.json"
done
ls-control scan-services 2>/dev/null || true

luna-send -n 1 -f luna://com.webos.appInstallService/dev/remove \
    '{"id":"org.webosbrew.lg-hue-sync","subscribe":false}' >/dev/null 2>&1 || true

# Launcher made by scripts/install_remote_shortcut.sh. That script creates the
# directory by hand, not through the app installer, so remove it by path. Act
# only on the real directory (not a symlink) whose appinfo.json has its id.
shortcut_dir=/media/developer/apps/usr/palm/applications/com.adeze.lghuesync
if [ -d "$shortcut_dir" ] && [ ! -L "$shortcut_dir" ] \
    && grep -q '"id": *"com.adeze.lghuesync"' "$shortcut_dir/appinfo.json" 2>/dev/null; then
    rm -rf "$shortcut_dir"
    luna-send -n 1 luna://com.webos.applicationManager/rescanAppList '{}' >/dev/null 2>&1 || true
    echo "Launcher com.adeze.lghuesync removed"
fi

if [ "$PURGE_CONFIG" = false ] && [ -f "$install_dir/config.json" ]; then
    mkdir -p "$backup_dir"
    cp -p "$install_dir/config.json" "$backup_dir/config.json"
    echo "Configuration preserved at $backup_dir/config.json"
fi
rm -rf "$install_dir"
systemctl daemon-reload 2>/dev/null || true
echo "lg-hue-sync removed"
REMOTE
