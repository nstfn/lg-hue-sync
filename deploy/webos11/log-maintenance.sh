#!/bin/sh
# Clear the lg-hue-sync log in place when it reaches 1 MiB (webOS 11 layout).
# Install as /var/home/root/lg-hue-sync/log-maintenance.sh, mode 755.
# Usage: log-maintenance.sh [log-file]
# The default is state/daemon.log. Units and the boot hook are installed by
# hand (see README.md), so this copy has no --install mode.
set -eu

[ "$#" -le 1 ] || { echo "Usage: $0 [log-file]" >&2; exit 2; }
log_file=${1:-/var/home/root/lg-hue-sync/state/daemon.log}
if [ -L "$log_file" ] || { [ -e "$log_file" ] && [ ! -f "$log_file" ]; }; then
    echo "Refusing to clear a symlink or non-regular log: $log_file" >&2
    exit 1
fi
[ -f "$log_file" ] || exit 0
size=$(stat -c %s "$log_file")
if [ "$size" -ge 1048576 ]; then
    # Keep the inode: PID 1 opened the daemon's stdout with O_APPEND, and
    # that descriptor keeps writing to this file.
    : > "$log_file"
fi
