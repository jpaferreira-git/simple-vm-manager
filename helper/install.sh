#!/bin/sh
# Installs (or updates) the Server Helper for Simple VM Manager on this XenServer or XCP-ng server.
# Free software under the MIT license, given as it is, with no warranty: read it, and svm-helper,
# before you run it. Run it as root, in the server's console or over SSH:
#
#   curl -fsSL https://raw.githubusercontent.com/jpaferreira-git/simple-vm-manager/<version>/helper/install.sh | sh -s <version>
#
# It fetches svm-helper from that same version, checks its SHA-256 against the one below, and puts:
#   /etc/xapi.d/plugins/svm-helper   the plugin Simple VM Manager talks to
#   /etc/cron.d/svm-helper           a look every minute
# Nothing else changes. Remove it from the app (the pool's Helper tab), or delete those two files.
set -eu
TAG="${1:-main}"
SHA256="a92755fb71dd5066438f1d52eb8ed2a9d9e95b18804b517c47eb87d93750548f"
URL="https://raw.githubusercontent.com/jpaferreira-git/simple-vm-manager/$TAG/helper/svm-helper"

[ "$(id -u)" = 0 ] || { echo "Run it as root."; exit 1; }
[ -d /etc/xapi.d/plugins ] || { echo "This isn't a XenServer or XCP-ng server: nothing was installed."; exit 1; }

TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT
curl -fsSL "$URL" -o "$TMP" || { echo "Couldn't fetch $URL: nothing was installed."; exit 1; }
echo "$SHA256  $TMP" | sha256sum -c --status - || { echo "The file's fingerprint doesn't match the one expected: nothing was installed."; exit 1; }

install -m 0755 "$TMP" /etc/xapi.d/plugins/svm-helper
mkdir -p /var/lib/svm-helper
printf '%s\n' '# Server Helper for Simple VM Manager: a look every minute' \
  '* * * * * root /etc/xapi.d/plugins/svm-helper --tick >/dev/null 2>&1' > /etc/cron.d/svm-helper
chmod 0644 /etc/cron.d/svm-helper
echo "Server Helper installed on $(hostname). Simple VM Manager sees it within a few seconds."
