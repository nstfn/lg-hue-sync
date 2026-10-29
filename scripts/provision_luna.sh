#!/usr/bin/env bash
set -euo pipefail

# provision_luna.sh
# Provisions Luna Service 2 security manifests, roles, and client permissions
# for org.webosbrew.lg-hue-sync on LG webOS 6.x (LG C1 OLED).
#
# The remote path sends this same file to the TV and runs it there with
# `local`, so one function writes the files on both paths.
# LUNA_PREFIX is a test hook: it prefixes the directories written, never the
# paths inside the JSON. Leave it unset on a TV.

TV_IP="${1:?Usage: ./scripts/provision_luna.sh <tv-ip|local> [ssh-port]}"
SSH_PORT="${2:-22}"
LUNA_PREFIX="${LUNA_PREFIX:-}"

SSH_OPTS="-p $SSH_PORT -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5"
NAME="org.webosbrew.lg-hue-sync"

# Write stdin to $1 through a mktemp file in the same directory, then mv.
# Root runs this, so it never uses a fixed name in a shared directory such as
# /tmp, and a reader never sees a partial file.
install_json() {
    local target="$1" tmp
    tmp="$(mktemp "$(dirname "$target")/.$NAME.XXXXXX")"
    if ! cat > "$tmp" || ! chmod 644 "$tmp" || ! mv -f "$tmp" "$target"; then
        rm -f "$tmp"
        return 1
    fi
}

manifest_json() {
    cat << JSON
{
    "id": "org.webosbrew.lg-hue-sync",
    "version": "1.0.0",
    "roleFiles": [
        "$1/roles.d/org.webosbrew.lg-hue-sync.json"
    ],
    "clientPermissionFiles": [
        "$1/client-permissions.d/org.webosbrew.lg-hue-sync.json"
    ],
    "serviceFiles": [],
    "apiPermissionFiles": []
}
JSON
}

# Privileged role file (without appId).
role_json() {
    cat << 'JSON'
{
    "exeName": "/var/home/root/lg-hue-sync/lg-hue-sync",
    "type": "privileged",
    "allowedNames": [
        "com.webos.service.capture",
        "com.webos.service.capture.client*",
        "com.webos.rm.client.*",
        "com.webos.service.capturepermission",
        "org.webosbrew.lg-hue-sync*"
    ],
    "permissions": [
        {
            "service": "com.webos.service.capture",
            "inbound": ["*"],
            "outbound": ["*"]
        },
        {
            "service": "com.webos.service.capture.client*",
            "inbound": ["*"],
            "outbound": ["*"]
        },
        {
            "service": "com.webos.rm.client.*",
            "inbound": ["*"],
            "outbound": ["*"]
        },
        {
            "service": "com.webos.service.capturepermission",
            "inbound": ["*"],
            "outbound": ["*"]
        },
        {
            "service": "org.webosbrew.lg-hue-sync*",
            "inbound": ["*"],
            "outbound": ["*"]
        }
    ]
}
JSON
}

perm_json() {
    cat << 'JSON'
{
    "org.webosbrew.lg-hue-sync*": [
        "all"
    ],
    "com.webos.service.capture.client*": [
        "all"
    ],
    "com.webos.service.capture": [
        "all"
    ],
    "com.webos.service.capturepermission": [
        "all"
    ],
    "com.webos.rm.client.*": [
        "all"
    ]
}
JSON
}

provision_files() {
    local root dir
    for root in /var/luna-service2 /var/luna-service2-dev; do
        dir="$LUNA_PREFIX$root"
        mkdir -p "$dir/manifests.d" "$dir/roles.d" "$dir/client-permissions.d"
        manifest_json "$root" | install_json "$dir/manifests.d/$NAME.json"
        role_json | install_json "$dir/roles.d/$NAME.json"
        perm_json | install_json "$dir/client-permissions.d/$NAME.json"
    done

    # stdin may be this script on the remote path: keep it from ls-control.
    ls-control scan-services < /dev/null
    echo "[+] Luna permissions provisioned and scanned successfully."
}

if [ "$TV_IP" = "local" ] || [ "$TV_IP" = "localhost" ] || [ "$TV_IP" = "127.0.0.1" ]; then
    echo "[*] Provisioning Luna permissions for $NAME locally..."
    provision_files
else
    echo "[*] Provisioning Luna permissions for $NAME on $TV_IP:$SSH_PORT..."
    # bash reads the script from stdin; every command above that could read
    # stdin gets a pipe or /dev/null instead.
    ssh $SSH_OPTS root@"$TV_IP" "bash -s -- local" < "${BASH_SOURCE[0]}"
fi
