#!/bin/bash
# Simple VM Manager's Helper VM: brings itself up to date when the app asks (its Update button).
#
# svm-helper-update.service runs it, as root, once the helper leaves the version to install in
# /var/lib/svm-helper/update.request (the helper itself runs unprivileged and can't install
# anything). It fetches that version's setup.sh from GitHub and runs it without questions, as
# 'sudo bash setup.sh' would, and leaves how it went in update.json for the helper to tell the app.
#
# Free software under the MIT license, given as it is, with no warranty.

set -uo pipefail
REPO=jpaferreira-git/simple-vm-manager
REQ=/var/lib/svm-helper/update.request
OUT=/var/lib/svm-helper/update.json
LOG=/var/log/svm-helper-update.log

[[ -f $REQ ]] || exit 0
ref=$(head -c 64 "$REQ" | tr -d '[:space:]')
rm -f "$REQ"

said() {  # said ok|failed "why"
  printf '{"ref": "%s", "at": %s, "ok": %s, "error": "%s"}\n' "$ref" "$(date +%s)" "$([[ $1 == ok ]] && echo true || echo false)" "$2" >"$OUT.new"
  chgrp svm-helper "$OUT.new" 2>/dev/null
  chmod 0640 "$OUT.new"
  mv -f "$OUT.new" "$OUT"
}

if ! [[ $ref =~ ^(main|v[0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
  said failed "not a version: $ref"
  exit 1
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "--- $(date '+%F %T') updating to $ref" >>"$LOG"
if ! curl -fsSL "https://raw.githubusercontent.com/$REPO/$ref/helper-vm/setup.sh" -o "$tmp/setup.sh" 2>>"$LOG"; then
  said failed "couldn't fetch its setup for $ref from GitHub"
  exit 1
fi
if SVM_HELPER_REF="$ref" SVM_HELPER_NO_QUESTIONS=1 bash "$tmp/setup.sh" </dev/null >>"$LOG" 2>&1; then
  said ok ""
else
  said failed "its setup stopped: see $LOG on the Helper VM"
  exit 1
fi
