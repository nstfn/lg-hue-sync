#!/usr/bin/env bash
set -euo pipefail

# deploy.sh
# Cross-compiles the native Rust daemon for LG webOS 32-bit ARM (Alpha 9 Gen 4)
# and deploys it over SSH to the LG C1 TV as a headless systemd service.

TV_IP="${1:?Usage: ./scripts/deploy.sh <tv-ip> [ssh-port] [--reuse] [--with-config]}"
SSH_PORT="${2:-22}"
TARGET="armv7-unknown-linux-gnueabi"
REMOTE_DIR="/var/home/root/lg-hue-sync"
REUSE=false
WITH_CONFIG=false
for option in "${@:3}"; do
    case "$option" in
        --reuse) REUSE=true ;;
        --with-config) WITH_CONFIG=true ;;
        *) echo "Unknown option: $option" >&2; exit 2 ;;
    esac
done

if $WITH_CONFIG && [ ! -f "config.json" ]; then
    echo "[-] --with-config requested but config.json is missing." >&2
    exit 1
fi

echo "==================================================================="
echo "       LG C1 Native Rust Hue Sync Deployment ($TARGET)            "
echo "==================================================================="

# 1. Build by default; reuse only when explicitly requested.
BINARY="target/$TARGET/release/lg-hue-sync"

if [ -f "$BINARY" ] && $REUSE; then
    echo "[+] Using existing compiled binary at $BINARY ($(du -h "$BINARY" | cut -f1))"
    echo "    (Pass --reuse as 3rd argument only after verifying it matches this source tree)"
else
    echo "[*] Compiling release binary for $TARGET using the default native SDK/Buster toolchain..."
    make build
fi

if [ ! -f "$BINARY" ]; then
    echo "[-] Error: Compiled binary not found at $BINARY"
    exit 1
fi

echo "[+] Binary ready: $(du -h "$BINARY" | cut -f1)"

# Fail local packaging before making any changes on the TV.
IPK_PATH=""
if [ -d "webos-app" ]; then
    IPK_PATH="$(uv run scripts/package_ipk.py --print-path)"
fi

# 2. Check TV reachability and root status
COMMON_OPTS="-o StrictHostKeyChecking=accept-new -o ConnectTimeout=5"
SSH_OPTS="-p $SSH_PORT $COMMON_OPTS"
SCP_OPTS="-P $SSH_PORT $COMMON_OPTS"

if ! nc -z -w 3 "$TV_IP" "$SSH_PORT" &>/dev/null; then
    echo "[-] Root SSH is unavailable at $TV_IP:$SSH_PORT; rooting is a separate owner action." >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Everything up to the swap runs while the service keeps running, so a failure
# here (upload, checksum, Luna provisioning, IPK install) leaves the old daemon up.
echo "[*] Connecting to root@$TV_IP:$SSH_PORT..."
ssh $SSH_OPTS root@"$TV_IP" "mkdir -p $REMOTE_DIR"

echo "[*] Uploading binary to TV..."
scp $SCP_OPTS "$BINARY" root@"$TV_IP":"$REMOTE_DIR/lg-hue-sync.new"
LOCAL_SHA="$(shasum -a 256 "$BINARY" | awk '{print $1}')"
REMOTE_SHA="$(ssh $SSH_OPTS root@"$TV_IP" "sha256sum $REMOTE_DIR/lg-hue-sync.new" | awk '{print $1}')"
if [ "$LOCAL_SHA" != "$REMOTE_SHA" ]; then
    echo "[-] Binary checksum mismatch; existing binary was not replaced." >&2
    exit 1
fi

if $WITH_CONFIG; then
    # Created with mode 600 under a temporary name; the swap moves it into place.
    echo "[*] Uploading local config.json..."
    ssh $SSH_OPTS root@"$TV_IP" "umask 077 && cat > $REMOTE_DIR/config.json.new && chmod 600 $REMOTE_DIR/config.json.new" < config.json
fi

# 3. Luna Service 2 provisioning: the script runs over SSH from this computer,
# so no root-executed copy stays in the install directory. An older deploy left
# one there for the boot hook; step 5 removes it.
"$SCRIPT_DIR/provision_luna.sh" "$TV_IP" "$SSH_PORT"

# 4. Package and deploy webOS Application to Home Dashboard ribbon
if [ -n "$IPK_PATH" ]; then
    echo "[*] Installing packaged webOS application for Home Dashboard..."
    scp $SCP_OPTS "$IPK_PATH" root@"$TV_IP":"/tmp/org.webosbrew.lg-hue-sync.ipk"
    ssh $SSH_OPTS root@"$TV_IP" "luna-send -n 1 -f luna://com.webos.appInstallService/dev/install '{\"id\":\"org.webosbrew.lg-hue-sync\", \"ipkUrl\":\"/tmp/org.webosbrew.lg-hue-sync.ipk\", \"subscribe\":false}' || true"
fi

# 5. Write the systemd unit and the webosbrew init.d boot hook (no start yet).
echo "[*] Installing persistent webosbrew autostart hook on TV..."
ssh $SSH_OPTS root@"$TV_IP" bash << 'REMOTEEOC'
set -e
cat << 'EOF' > /var/home/root/lg-hue-sync/lg-hue-sync.service
[Unit]
Description=LG C1 Native Philips Hue & Nanoleaf Sync Daemon
After=network.target
Conflicts=sleep.target suspend.target
# Stop a crash loop: at most 10 starts in 30 minutes.
StartLimitIntervalSec=30min
StartLimitBurst=10

[Service]
Type=simple
WorkingDirectory=/var/home/root/lg-hue-sync
ExecStart=/bin/sh -c 'exec /var/home/root/lg-hue-sync/lg-hue-sync run --config /var/home/root/lg-hue-sync/config.json >> /var/home/root/lg-hue-sync/daemon.log 2>&1'
# always, not on-failure: after pairing the daemon exits 0 on purpose and
# relies on systemd to start it again with the new configuration.
Restart=always
RestartSec=5
# At shutdown the daemon may need about 7 s to stop the Hue stream and turn
# the lights off.
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
EOF

# Boot hook: Luna files live under /var and persist across reboots, so the hook
# does not provision them again; the unit goes to /run, which is a tmpfs.
mkdir -p /var/lib/webosbrew/init.d
cat << 'EOF' > /var/lib/webosbrew/init.d/50-lg-hue-sync.tmp
#!/bin/sh
# touch /var/home/root/lg-hue-sync/DISABLE to skip the start at boot.
D=/var/home/root/lg-hue-sync
U=lg-hue-sync.service
[ ! -e "$D/DISABLE" ] || exit 0
[ -x "$D/lg-hue-sync" ] || exit 0
[ -f "$D/$U" ] || exit 0
mkdir -p /run/systemd/system
cp -f "$D/$U" "/run/systemd/system/$U"
systemctl daemon-reload
systemctl --no-block start "$U"
EOF
chmod 755 /var/lib/webosbrew/init.d/50-lg-hue-sync.tmp
mv -f /var/lib/webosbrew/init.d/50-lg-hue-sync.tmp /var/lib/webosbrew/init.d/50-lg-hue-sync
rm -f /var/home/root/lg-hue-sync/provision_luna.sh
REMOTEEOC

# 6. Stop, swap and start in one remote command; it verifies the SHA-256 again,
# keeps a rollback binary, and starts the previous binary if the swap fails.
echo "[*] Swapping binary and restarting the service..."
ssh $SSH_OPTS root@"$TV_IP" "SHA=$LOCAL_SHA WITH_CONFIG=$WITH_CONFIG INSTALL_UNIT=true sh -s" < "$SCRIPT_DIR/swap_binary.sh"

scp $SCP_OPTS "$SCRIPT_DIR/log-maintenance.sh" root@"$TV_IP":"$REMOTE_DIR/log-maintenance.sh"
ssh $SSH_OPTS root@"$TV_IP" "chmod 755 $REMOTE_DIR/log-maintenance.sh && $REMOTE_DIR/log-maintenance.sh --install"

echo ""
echo "==================================================================="
echo "[+] SUCCESS: lg-hue-sync deployed and active on TV!"
echo "==================================================================="
echo "Management commands on TV (via SSH):"
echo "  Status:  ssh -p $SSH_PORT root@$TV_IP 'systemctl status lg-hue-sync'"
echo "  Logs:    ssh -p $SSH_PORT root@$TV_IP 'tail -f /var/home/root/lg-hue-sync/daemon.log'"
echo "  Restart: ssh -p $SSH_PORT root@$TV_IP 'systemctl restart lg-hue-sync'"
echo "  Stop:    ssh -p $SSH_PORT root@$TV_IP 'systemctl stop lg-hue-sync'"
echo "==================================================================="
