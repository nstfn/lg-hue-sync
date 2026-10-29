#!/bin/sh
# swap_binary.sh: runs ON THE TV as root, sent over SSH by scripts/deploy.sh and
# `make deploy-bin`: ssh root@<tv> "SHA=<sha256> sh -s" < scripts/swap_binary.sh
# Expects $D/lg-hue-sync.new to be uploaded already. The service is stopped only
# for the swap and started again in this same run; on any failure after the
# stop, the previous binary is put back and started.
# Optional environment:
#   WITH_CONFIG=true   also move $D/config.json.new into place
#   INSTALL_UNIT=true  copy $D/lg-hue-sync.service to /run/systemd/system
# POSIX sh only: the TV may run busybox ash.
set -eu

: "${SHA:?SHA (expected SHA-256 of lg-hue-sync.new) is required}"
WITH_CONFIG="${WITH_CONFIG:-false}"
INSTALL_UNIT="${INSTALL_UNIT:-false}"
D="${LG_HUE_SYNC_DIR:-/var/home/root/lg-hue-sync}"
U=lg-hue-sync.service
UNIT_DIR="${LG_HUE_SYNC_UNIT_DIR:-/run/systemd/system}"
BIN="$D/lg-hue-sync"

sha_of() {
    sha256sum "$1" | awk '{print $1}'
}

# 1. Verify the upload before anything changes.
if [ ! -f "$BIN.new" ] || [ "$(sha_of "$BIN.new")" != "$SHA" ]; then
    echo "[-] lg-hue-sync.new is missing or its SHA-256 does not match; nothing changed." >&2
    exit 1
fi
if [ "$WITH_CONFIG" = true ] && [ ! -f "$D/config.json.new" ]; then
    echo "[-] config.json.new is missing; nothing changed." >&2
    exit 1
fi
chmod 755 "$BIN.new"

# 2. Keep a good rollback copy. Replace .previous only when the running binary
# differs from the new one: redeploying the same build twice must not overwrite
# the last different binary with a copy of the new one.
rotated=false
if [ -f "$BIN" ] && [ "$(sha_of "$BIN")" != "$SHA" ]; then
    cp -f "$BIN" "$BIN.previous.tmp"
    chmod 755 "$BIN.previous.tmp"
    mv -f "$BIN.previous.tmp" "$BIN.previous"
    rotated=true
fi

# 3. Stop, swap, start. From here on, a failure restores and starts the old binary.
moved=false
started=false
rollback() {
    [ "$started" = false ] || return 0
    echo "[-] Swap failed; starting the previous binary again." >&2
    if [ "$moved" = true ] && [ "$rotated" = true ]; then
        cp -f "$BIN.previous" "$BIN.rollback.tmp" && mv -f "$BIN.rollback.tmp" "$BIN" || true
    fi
    systemctl start "$U" || true
}
trap rollback EXIT

systemctl stop "$U" 2>/dev/null || true
mv -f "$BIN.new" "$BIN"
moved=true
if [ "$WITH_CONFIG" = true ]; then
    mv -f "$D/config.json.new" "$D/config.json"
fi
if [ "$INSTALL_UNIT" = true ]; then
    mkdir -p "$UNIT_DIR"
    cp -f "$D/$U" "$UNIT_DIR/$U"
    systemctl daemon-reload
fi
systemctl start "$U"
started=true
trap - EXIT
echo "[TV] lg-hue-sync swapped and started (sha256 $SHA)."
