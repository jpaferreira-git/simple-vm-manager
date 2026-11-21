#!/usr/bin/env bash
# Prepares a Linux machine (a small VM is ideal) as the Helper VM of Simple VM Manager: it looks
# after your XenServer / XCP-ng pool from outside it, through the pool's API. Nothing is installed
# on the servers.
#
# Free software under the MIT license, given as it is, with no warranty: read it before you run it.
# Debian 12+, Ubuntu 22.04+, Fedora, Rocky/Alma 9+ or Arch. Run it as root (or with sudo):
#
#   curl -fsSL https://raw.githubusercontent.com/jpaferreira-git/simple-vm-manager/main/helper-vm/setup.sh -o setup.sh
#   sudo bash setup.sh
#
# Or from a copy of the repository: sudo bash helper-vm/setup.sh
#
# It installs Python and the NFS client from your system's packages, then:
#   /opt/svm-helper/                 the helper (svm-helper.py, its Python packages in venv/)
#   /usr/local/bin/svm-helper        its command
#   /etc/svm-helper/                 its settings (config.json, secrets.json: root and the helper only)
#   /var/lib/svm-helper/             what it keeps (its log, state, last review)
#   svm-helper.service               the service, as the unprivileged user svm-helper
# and asks its questions (svm-helper setup). Run it again to update the helper; your answers stay.
set -euo pipefail

REF="${SVM_HELPER_REF:-main}"
REPO="jpaferreira-git/simple-vm-manager"
RAW="https://raw.githubusercontent.com/$REPO/$REF/helper-vm"
APP=/opt/svm-helper
LOG=/var/log/svm-helper-setup.log
TOTAL=7

# ---------------------------------------------------------------- looks
if [[ -t 1 && "${TERM:-dumb}" != dumb && -z "${NO_COLOR:-}" ]]; then
  B=$'\e[1m'; D=$'\e[2m'; R=$'\e[31m'; G=$'\e[32m'; Y=$'\e[33m'; BL=$'\e[1;34m'; C=$'\e[36m'; N=$'\e[0m'; FANCY=1
else
  B=; D=; R=; G=; Y=; BL=; C=; N=; FANCY=
fi

banner() {
  local title="$1" sub="$2" w=$(( ${#2} > ${#1} ? ${#2} : ${#1} ))
  w=$((w + 6))
  local line; line=$(printf '─%.0s' $(seq 1 "$w"))
  echo
  echo "  ${BL}╭${line}╮${N}"
  printf "  ${BL}│${N}   ${B}%-*s${N}${BL}│${N}\n" $((w - 3)) "$title"
  printf "  ${BL}│${N}   ${D}%-*s${N}${BL}│${N}\n" $((w - 3)) "$sub"
  echo "  ${BL}╰${line}╯${N}"
}
step() { echo; echo "  ${BL}[$1/$TOTAL]${N} ${B}$2${N}"; }
ok()   { echo "        ${G}✓${N} $*"; }
warn() { echo "        ${Y}!${N} $*"; }
fail() { echo "        ${R}✗${N} $*"; }
info() { echo "        ${C}›${N} $*"; }
note() { echo "$*" | fold -s -w 74 | sed "s/^/          ${D}/; s/\$/${N}/"; }
die()  { fail "$*"; echo; echo "  ${D}What it did is in $LOG${N}"; echo; exit 1; }

# run "text" command...: a spinner while it works, its output in the log; ✓ or ✗ after
run() {
  local text="$1"; shift
  echo "--- $(date '+%F %T') $text: $*" >>"$LOG"
  "$@" >>"$LOG" 2>&1 &
  local pid=$! i=0 frames='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
  if [[ -n "$FANCY" ]]; then
    while kill -0 "$pid" 2>/dev/null; do
      printf "\r        ${C}%s${N} %s " "${frames:i++%10:1}" "$text"
      sleep 0.1
    done
    printf "\r\e[K"
  fi
  if wait "$pid"; then
    ok "$text"
  else
    fail "$text"
    echo "${D}"; tail -n 8 "$LOG" | sed 's/^/          /'; echo -n "${N}"
    return 1
  fi
}

ask_yes() {  # ask_yes "question" default(y/n)
  local a
  while true; do
    read -r -p "        ${C}?${N} $1 ${D}(y/n) [$2]${N}: " a </dev/tty || a=""
    a="${a:-$2}"
    case "${a,,}" in y|yes|s|sim) return 0 ;; n|no|nao|não) return 1 ;; esac
  done
}

trap 'echo; fail "Stopped."; exit 130' INT

banner "Helper VM for Simple VM Manager" "looks after your XenServer pool from this machine"
note "This prepares this machine: a few packages, the helper and its service. Then it asks about your pool, where alerts go, backups and Claude. Enter takes what's in [brackets]."

# ---------------------------------------------------------------- 1. this machine
step 1 "This machine"
[[ $EUID -eq 0 ]] || die "Run it as root: sudo bash $0"
: >"$LOG"; chmod 600 "$LOG"
command -v systemctl >/dev/null && [[ -d /run/systemd/system ]] || die "It needs systemd, and this machine doesn't run it."
. /etc/os-release
ok "${PRETTY_NAME:-$ID} · $(uname -m) · $(hostname)"
case " ${ID:-} ${ID_LIKE:-} " in
  *" debian "*|*" ubuntu "*) PM=apt ;;
  *" fedora "*|*" rhel "*|*" centos "*) PM=dnf ;;
  *" arch "*) PM=pacman ;;
  *) die "This system ($ID) isn't one it knows: use Debian, Ubuntu, Fedora, Rocky/Alma or Arch." ;;
esac
if [[ "$(cat /sys/hypervisor/type 2>/dev/null)" == xen ]]; then
  ok "A VM on Xen: it can find itself in the pool"
else
  info "Not a Xen VM: fine, it looks after the pool from outside"
fi
for site in github.com pypi.org claude.ai api.anthropic.com; do
  if curl -sS -m 8 -o /dev/null "https://$site" 2>>"$LOG"; then
    ok "Reaches $site"
  else
    case $site in
      claude.ai|api.anthropic.com) warn "Doesn't reach $site: Claude won't work from here" ;;
      *) die "Doesn't reach $site: check this machine's internet." ;;
    esac
  fi
done
free_kb=$(df -Pk /opt | awk 'NR==2 {print $4}')
(( free_kb > 600000 )) && ok "$((free_kb / 1024)) MB free" || die "Only $((free_kb / 1024)) MB free on /opt: it needs about 600 MB."
tz=$(timedatectl show -p Timezone --value 2>/dev/null || echo UTC)
info "Time zone: ${B}$tz${N}  ${D}(backups and the weekly review go by it)${N}"
if [[ "$tz" == "UTC" || "$tz" == "Etc/UTC" ]]; then
  read -r -p "        ${C}?${N} Your time zone ${D}(like America/Sao_Paulo; Enter keeps UTC)${N}: " newtz </dev/tty || newtz=""
  if [[ -n "$newtz" ]]; then
    timedatectl set-timezone "$newtz" 2>>"$LOG" && ok "Time zone: $newtz" || warn "Couldn't set $newtz (see 'timedatectl list-timezones')"
  fi
fi

# ---------------------------------------------------------------- 2. what it needs
step 2 "Installing what it needs"
case $PM in
  apt)
    export DEBIAN_FRONTEND=noninteractive
    run "Refreshing the package lists" apt-get update -q || die "apt-get update failed."
    run "Python, the NFS client and the rest" apt-get install -y -q python3 python3-venv nfs-common ca-certificates curl || die "The packages couldn't be installed."
    if apt-cache show xe-guest-utilities >/dev/null 2>&1; then
      run "XenServer's guest tools (the pool sees this VM's address)" apt-get install -y -q xe-guest-utilities || warn "Skipped the guest tools"
    fi
    systemctl enable --now systemd-timesyncd >>"$LOG" 2>&1 || true
    ;;
  dnf)
    run "Python, the NFS client and the rest" dnf install -y -q python3 nfs-utils ca-certificates curl || die "The packages couldn't be installed."
    systemctl enable --now chronyd >>"$LOG" 2>&1 || true
    ;;
  pacman)
    run "Python, the NFS client and the rest" pacman -Sy --noconfirm --needed python nfs-utils ca-certificates curl || die "The packages couldn't be installed."
    systemctl enable --now systemd-timesyncd >>"$LOG" 2>&1 || true
    ;;
esac
new_enough() { "$1" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)' 2>/dev/null; }
PY=""
for p in python3 python3.13 python3.12 python3.11 python3.10; do
  command -v "$p" >/dev/null && new_enough "$(command -v "$p")" && { PY=$(command -v "$p"); break; }
done
if [[ -z "$PY" && $PM == dnf ]]; then
  run "Python 3.11 (the system's own is older)" dnf install -y -q python3.11 && PY=$(command -v python3.11) || true
fi
[[ -n "$PY" ]] || die "It needs Python 3.10 or newer; this system has $(python3 -V 2>&1). Use Debian 12, Ubuntu 22.04 or newer."
ok "$("$PY" -V)"

getent group svm-helper >/dev/null || groupadd --system svm-helper
if ! id svm-helper >/dev/null 2>&1; then
  useradd --system --gid svm-helper --home-dir /var/lib/svm-helper --no-create-home --shell /usr/sbin/nologin svm-helper
  ok "User svm-helper (the service runs as it, without root)"
fi
install -d -m 0755 "$APP"
install -d -m 0750 -o root -g svm-helper /etc/svm-helper
install -d -m 2770 -o svm-helper -g svm-helper /var/lib/svm-helper
mountpoint -q /srv/svm-backups 2>/dev/null || install -d -m 0770 -o svm-helper -g svm-helper /srv/svm-backups

# its files: from this copy of the repository, or from GitHub
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || echo /nonexistent)"
for f in svm-helper.py svm-helper.service CLAUDE.md; do
  if [[ -f "$HERE/$f" ]]; then
    install -m 0644 "$HERE/$f" "$APP/$f.new"
  else
    curl -fsSL "$RAW/$f" -o "$APP/$f.new" 2>>"$LOG" || die "Couldn't fetch $RAW/$f"
  fi
  mv -f "$APP/$f.new" "$APP/$f"
done
[[ -f "$HERE/svm-helper.py" ]] && ok "The helper, from $HERE" || ok "The helper, from GitHub ($REF)"
chmod 0755 "$APP/svm-helper.py"

if [[ ! -x "$APP/venv/bin/python" ]]; then
  run "Its own Python environment" "$PY" -m venv "$APP/venv" || die "Couldn't make the Python environment."
fi
run "Its Python packages (XenAPI, Anthropic's SDK)" "$APP/venv/bin/pip" install -q --upgrade pip XenAPI anthropic || die "Couldn't install the Python packages."

cat >/usr/local/bin/svm-helper <<EOF
#!/bin/sh
exec $APP/venv/bin/python $APP/svm-helper.py "\$@"
EOF
chmod 0755 /usr/local/bin/svm-helper
install -m 0644 "$APP/svm-helper.service" /etc/systemd/system/svm-helper.service
systemctl daemon-reload
systemctl try-restart svm-helper >>"$LOG" 2>&1 || true
ok "Its command (svm-helper) and its service"
version=$(/usr/local/bin/svm-helper version 2>>"$LOG" || echo "?")
ok "Helper VM $version"

# ---------------------------------------------------------------- 3 to 7: the questions
if [[ -n "${SVM_HELPER_NO_QUESTIONS:-}" ]]; then
  echo; info "Questions skipped: run ${B}sudo svm-helper setup${N} when you're ready."; echo
  exit 0
fi
exec /usr/local/bin/svm-helper setup --from-installer </dev/tty
