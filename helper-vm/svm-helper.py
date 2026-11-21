#!/usr/bin/env python3
# Helper VM for Simple VM Manager (https://github.com/jpaferreira-git/simple-vm-manager)
#
# Free software under the MIT license, given as it is, with no warranty: read it before you run it.
#
# It runs on a small Linux VM (or any Linux machine on the pool's network) and looks after a
# XenServer / XCP-ng pool from outside it, through the pool's own API (XAPI over HTTPS, the
# servers' certificates pinned at setup). Nothing is installed on the servers.
#
#   helper-vm/setup.sh       prepares the machine, then runs "svm-helper setup"
#   svm-helper setup         the questions (run it again any time to change the answers)
#   svm-helper.service       the service, as the unprivileged user svm-helper
#
# Each minute it:
#  - sends the pool's new alerts to Telegram, ntfy, Discord, Home Assistant or any webhook,
#    explained in plain words by Claude when they're serious (with an Anthropic API key);
#  - knocks on each server, and says when one stops answering, and when it's back;
#  - says when it can't reach the pool at all (straight to your phone: the pool can't);
#  - watches the services you name (Home Assistant, the NAS...) and says when one is down;
#  - raises an alert when a storage is fuller than its limit (any kind of storage);
#  - makes up a snapshot schedule's run that was missed (XenServer's plugin never does);
#  - mounts shared storage again on a server that started before the NAS was there;
#  - starts the VMs on its list in order, once, after the pool starts.
# Each hour it compares the servers' clocks with the right time.
# When Simple VM Manager converts VMs from VMware or Proxmox, it copies their disks straight from
# the source into the pool (a few seconds after the app asks), so they don't pass through the
# computer the app runs on.
# On its schedule it backs up VMs (.xva) and the pool's database to a NAS share, and writes a
# weekly health review of the pool (by Claude, or a plain summary without a key).
#
# Its settings: /etc/svm-helper/config.json and secrets.json (the pool's password, the API key),
# plus the pool's other_config "svm-helper" (where alerts go, and what the app's Helper tab
# sets). Its heartbeat goes in the pool's other_config "svm-helper-vm", for the app to show.

import base64
import calendar
import code
import codecs
import datetime
import fcntl
import getpass
import hashlib
import html
import http.client
import http.server
import json
import os
import pwd
import grp
import re
import secrets as pysecrets
import shutil
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import termios
import textwrap
import threading
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
import xmlrpc.client

VERSION = "2.3.0"
REPO = "jpaferreira-git/simple-vm-manager"
CONF_DIR = os.environ.get("SVM_HELPER_CONF", "/etc/svm-helper")
CONFIG = os.path.join(CONF_DIR, "config.json")
SECRETS = os.path.join(CONF_DIR, "secrets.json")
STATE_DIR = os.environ.get("SVM_HELPER_STATE", "/var/lib/svm-helper")
APP_DIR = os.environ.get("SVM_HELPER_APP", "/opt/svm-helper")
USER = "svm-helper"
SERVICE = "svm-helper"
SETTINGS_KEY = "svm-helper"
STATUS_KEY = "svm-helper-vm"
MODEL = "claude-opus-5-5"
CLAUDE_HOME = os.path.join(STATE_DIR, "claude")  # where Claude Code is installed for the helper

PRIORITY_WORDS = {1: "Critical", 2: "Major", 3: "Warning", 4: "Minor", 5: "Info"}
PRIORITY_MARK = {1: "🔴", 2: "🔴", 3: "🟠", 4: "🔵", 5: "🔵"}
DAYS = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]
LANGUAGES = {"pt-BR": "Brazilian Portuguese", "en": "English", "es": "Spanish"}

DEFAULT_CONFIG = {
    "pool": {"hosts": [], "user": "root", "pins": []},
    "language": "en",
    "self_vm": "",
    "watch": {"hosts": True, "clocks": True, "services": []},
    "claude": {"auth": "", "model": MODEL, "cli_model": "", "explain": True, "explain_priority": 2, "explain_per_day": 10, "review": {"on": True, "day": "Monday", "at": "08:00"}, "advice": {"on": True, "at": "07:00"}},
}

AS_SERVICE = False


# ---------------------------------------------------------------- the terminal, made readable

class UI:
    """Colours, steps, questions and spinners for the terminal (plain when it isn't one)."""

    def __init__(self):
        self.color = sys.stdout.isatty() and os.environ.get("TERM") != "dumb" and "NO_COLOR" not in os.environ
        # The Linux console's fonts (Debian's Lat15 and Uni2) have neither ✓, braille nor half
        # blocks: plain marks there, and a QR code of full blocks.
        self.console = os.environ.get("TERM") == "linux"
        self.tick, self.cross = ("+", "x") if self.console else ("✓", "✗")
        self.frames = "|/-\\" if self.console else "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"

    def c(self, code_, text):
        return "\033[%sm%s\033[0m" % (code_, text) if self.color else text

    def bold(self, t):
        return self.c("1", t)

    def dim(self, t):
        return self.c("2", t)

    def green(self, t):
        return self.c("32", t)

    def red(self, t):
        return self.c("31", t)

    def yellow(self, t):
        return self.c("33", t)

    def blue(self, t):
        return self.c("1;34", t)

    def cyan(self, t):
        return self.c("36", t)

    def banner(self, title, subtitle=""):
        width = max(len(title), len(subtitle)) + 6
        print()
        print("  " + self.blue("╭" + "─" * width + "╮"))
        print("  " + self.blue("│") + "   " + self.bold(title.ljust(width - 3)) + self.blue("│"))
        if subtitle:
            print("  " + self.blue("│") + "   " + self.dim(subtitle.ljust(width - 3)) + self.blue("│"))
        print("  " + self.blue("╰" + "─" * width + "╯"))

    def step(self, n, total, title):
        print("\n  " + self.blue("[%d/%d]" % (n, total)) + " " + self.bold(title))

    def ok(self, text):
        print("        " + self.green(self.tick) + " " + text)

    def warn(self, text):
        print("        " + self.yellow("!") + " " + text)

    def fail(self, text):
        print("        " + self.red(self.cross) + " " + text)

    def info(self, text):
        print("        " + self.cyan("›") + " " + text)

    def note(self, text):
        for line in textwrap.wrap(text, 74):
            print("          " + self.dim(line))

    def ask(self, prompt, default="", secret=False, check=None, keep_hint=None):
        """A typed answer: Enter takes the default; `check` says what's wrong with one, if anything."""
        while True:
            hint = keep_hint if keep_hint is not None else default
            shown = "        " + self.cyan("?") + " " + prompt + (self.dim(" [%s]" % hint) if hint else "") + ": "
            try:
                answer = getpass.getpass(shown) if secret else input(shown)
            except EOFError:
                print()
                raise KeyboardInterrupt
            answer = answer.strip() or default
            why = check(answer) if check else None
            if not why:
                return answer
            self.fail(why)

    def yes(self, prompt, default=True):
        while True:
            a = self.ask(prompt + " " + self.dim("(y/n)"), "y" if default else "n").lower()
            if a in ("y", "yes", "s", "sim"):
                return True
            if a in ("n", "no", "nao", "não"):
                return False

    def pick(self, prompt, options, default=0):
        for i, o in enumerate(options):
            print("          " + self.bold("%d" % (i + 1)) + "  " + o)
        a = self.ask(prompt, str(default + 1), check=lambda v: None if v.isdigit() and 1 <= int(v) <= len(options) else "type a number from the list")
        return int(a) - 1

    def picks(self, prompt, options, default=()):
        """Several from a numbered list: "1 3 5", "all", or "none"."""
        for i, o in enumerate(options):
            print("          " + self.bold("%2d" % (i + 1)) + "  " + o)

        def parse(v):
            v = v.strip().lower()
            if v in ("all", "todos", "todas"):
                return list(range(len(options)))
            if v in ("", "none", "nenhum", "nenhuma", "-"):
                return []
            out = []
            for part in re.split(r"[ ,]+", v):
                if "-" in part and all(p.isdigit() for p in part.split("-", 1)):
                    a, b = (int(p) for p in part.split("-", 1))
                    out += list(range(a - 1, b))
                elif part.isdigit():
                    out.append(int(part) - 1)
                else:
                    return None
            return out if all(0 <= i < len(options) for i in out) else None

        hint = " ".join(str(i + 1) for i in default) or "none"
        a = self.ask(prompt, hint, check=lambda v: None if parse(v) is not None else "type numbers from the list (like 1 3 5), all, or none")
        return sorted(set(parse(a)))

    def spin(self, text, fn):
        """Runs fn() with a spinner beside `text`; its result, or its exception raised again."""
        out = {}

        def run():
            try:
                out["value"] = fn()
            except BaseException as e:
                out["error"] = e

        t = threading.Thread(target=run, daemon=True)
        t.start()
        i = 0
        while t.is_alive():
            if self.color:
                sys.stdout.write("\r        " + self.cyan(self.frames[i % len(self.frames)]) + " " + text + " ")
                sys.stdout.flush()
            t.join(0.1)
            i += 1
        if self.color:
            sys.stdout.write("\r\033[K")
        if "error" in out:
            self.fail(text + ": " + short_error(out["error"]))
            raise out["error"]
        self.ok(text)
        return out.get("value")

    def box(self, title, lines):
        width = max([len(title) + 4] + [len(strip_ansi(l)) for l in lines]) + 4
        print()
        print("  " + self.green("╭─ ") + self.bold(title) + " " + self.green("─" * (width - len(title) - 3) + "╮"))
        for l in lines:
            pad = width - len(strip_ansi(l))
            print("  " + self.green("│") + "  " + l + " " * (pad - 2) + self.green("│"))
        print("  " + self.green("╰" + "─" * width + "╯"))


def strip_ansi(text):
    return re.sub(r"\033\[[0-9;]*m", "", text)


def short_error(e):
    if isinstance(e, KeyboardInterrupt):
        return "stopped"
    text = str(e) or e.__class__.__name__
    return text if len(text) < 300 else text[:297] + "..."


ui = UI()


# ---------------------------------------------------------------- small things

def log(text):
    """One line in the log (and on the service's output); the log is cut to its newer half past
    a megabyte."""
    line = "%s %s" % (time.strftime("%Y-%m-%d %H:%M:%S"), text)
    if AS_SERVICE:
        print(text, flush=True)
    path = os.path.join(STATE_DIR, "helper.log")
    try:
        with open(path, "a") as f:
            f.write(line + "\n")
        if os.path.getsize(path) > 1024 * 1024:
            with open(path) as f:
                lines = f.readlines()
            with open(path, "w") as f:
                f.writelines(lines[len(lines) // 2:])
    except OSError:
        pass


def log_tail(n=20):
    try:
        with open(os.path.join(STATE_DIR, "helper.log")) as f:
            return [l.rstrip("\n") for l in f.readlines()[-n:]]
    except OSError:
        return []


def load_json(path, default):
    try:
        with open(path) as f:
            v = json.load(f)
        return v if isinstance(v, type(default)) else default
    except (OSError, ValueError):
        return default


def save_json(path, data, mode=0o660):
    tmp = path + ".new"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=1, sort_keys=True)
    os.chmod(tmp, mode)
    os.replace(tmp, path)


def merged(base, over):
    """`over` laid on `base`, dict by dict."""
    out = dict(base)
    for k, v in (over or {}).items():
        out[k] = merged(base[k], v) if isinstance(v, dict) and isinstance(base.get(k), dict) else v
    return out


def load_config():
    return merged(DEFAULT_CONFIG, load_json(CONFIG, {}))


def load_secrets():
    return load_json(SECRETS, {})


def state_path(name):
    return os.path.join(STATE_DIR, name + ".json")


def on(cfg, *path, default=False):
    """A setting's on/off, as a path of keys into the settings."""
    v = cfg
    for k in path:
        if not isinstance(v, dict):
            return default
        v = v.get(k)
    return default if v is None else bool(v)


def xen_epoch(value):
    """A XenAPI date ("20261120T18:43:54Z", UTC) as seconds since 1970; 0 when it can't be read."""
    try:
        text = value.value if hasattr(value, "value") else str(value)
        text = text.strip().rstrip("Z").replace("-", "")
        if "." in text:
            text = text.split(".", 1)[0]
        return calendar.timegm(time.strptime(text, "%Y%m%dT%H:%M:%S"))
    except Exception:
        return 0


def xen_date(epoch):
    return xmlrpc.client.DateTime(time.strftime("%Y%m%dT%H:%M:%SZ", time.gmtime(epoch)))


def clock(epoch):
    return time.strftime("%H:%M", time.localtime(epoch))


def gb(n):
    n = float(n)
    if n >= 1024 ** 4:
        return "%.1f TB" % (n / 1024 ** 4)
    return "%.0f GB" % (n / 1024 ** 3) if n >= 10 * 1024 ** 3 else "%.1f GB" % (n / 1024 ** 3)


def span(seconds):
    """A length of time in words: 40 s, 12 min, 3 h 5 min, 2 days."""
    s = int(max(0, seconds))
    if s < 90:
        return "%d s" % s
    if s < 3600:
        return "%d min" % round(s / 60)
    if s < 86400 * 2:
        h, m = divmod(round(s / 60), 60)
        return "%d h %d min" % (h, m) if m else "%d h" % h
    return "%d days" % round(s / 86400)


def bracket(address):
    """An address for a URL: an IPv6 one in brackets (a name, IPv4, or either with :port, as is)."""
    return "[%s]" % address if address.count(":") > 1 and not address.startswith("[") else address


def split_port(address, port=443):
    """(host, port) from "host", "host:port", "fe80::1" or "[fe80::1]:port"."""
    if address.startswith("[") and "]" in address:
        host, _, rest = address[1:].partition("]")
        return host, int(rest[1:]) if rest.startswith(":") and rest[1:].isdigit() else port
    if address.count(":") == 1:
        host, p = address.split(":")
        return host, int(p) if p.isdigit() else port
    return address, port


def tcp_ok(address, port=443, timeout=3.0):
    try:
        with socket.create_connection(split_port(address, port), timeout=timeout):
            return True
    except OSError:
        return False


def my_address(towards):
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.connect((towards, 443))
            return s.getsockname()[0]
    except OSError:
        return ""


def this_vm_uuid():
    """This machine's VM uuid, when it's a VM on Xen (/sys/hypervisor/uuid, or the firmware's)."""
    for path in ("/sys/hypervisor/uuid", "/sys/class/dmi/id/product_uuid"):
        try:
            with open(path) as f:
                u = f.read().strip().lower()
            if re.fullmatch(r"[0-9a-f-]{36}", u) and u != "00000000-0000-0000-0000-000000000000":
                return u
        except OSError:
            pass
    return ""


def ntp_synced():
    """Whether this machine's clock is kept right (None when it can't be told)."""
    try:
        out = subprocess.run(["timedatectl", "show", "-p", "NTPSynchronized", "--value"], capture_output=True, text=True, timeout=5).stdout.strip()
        return {"yes": True, "no": False}.get(out)
    except Exception:
        return None


def safe_name(name):
    return re.sub(r"[^A-Za-z0-9._-]+", "-", name).strip("-")[:60] or "vm"


def due(slot_at, days, last, now, window=6 * 3600):
    """Whether a job set for HH:MM on `days` (every day when empty), by this machine's clock,
    is due: its latest time has gone by, it hasn't run since, and it's less than `window` late."""
    try:
        h, m = (int(p) for p in str(slot_at).split(":", 1))
    except ValueError:
        return False
    t = datetime.datetime.fromtimestamp(now)
    for back in range(0, 8):
        cand = (t - datetime.timedelta(days=back)).replace(hour=h, minute=m, second=0, microsecond=0)
        if cand > t:
            continue
        if days and cand.strftime("%A") not in days:
            continue
        slot = time.mktime(cand.timetuple())
        return last < slot and now - slot < window
    return False


# ---------------------------------------------------------------- the pool, over pinned HTTPS

class CertificateChanged(Exception):
    def __init__(self, host, fingerprint):
        Exception.__init__(self, "%s answered with a certificate this helper doesn't know (%s...): run 'sudo svm-helper trust' if you changed it" % (host, fingerprint[:16]))
        self.host = host
        self.fingerprint = fingerprint


class PoolError(Exception):
    pass


def unverified_context():
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return ctx


def cert_fingerprint(address, port=443, timeout=6):
    """The SHA-256 of the certificate a server answers HTTPS with."""
    host, port = split_port(address, port)
    with socket.create_connection((host, port), timeout=timeout) as sock:
        with unverified_context().wrap_socket(sock, server_hostname=host) as s:
            return hashlib.sha256(s.getpeercert(binary_form=True)).hexdigest()


def pretty_fingerprint(fp):
    """In two lines, as openssl shows it (AB:CD:...), to compare with the server's own."""
    pairs = [fp[i:i + 2].upper() for i in range(0, len(fp), 2)]
    return ":".join(pairs[:16]), ":".join(pairs[16:])


class PinnedConnection(http.client.HTTPSConnection):
    """HTTPS to a pool's server, accepted only when its certificate is one of `pins` (the servers
    have self-signed ones: they're pinned at setup instead of checked by an authority)."""

    def __init__(self, host, pins, timeout=600):
        http.client.HTTPSConnection.__init__(self, host, timeout=timeout, context=unverified_context())
        self.pins = pins

    def connect(self):
        # a server that's off is given up on quickly; one that's working may take its time
        io = self.timeout
        self.timeout = min(io, 8)
        try:
            http.client.HTTPSConnection.connect(self)
        finally:
            self.timeout = io
        self.sock.settimeout(io)
        fp = hashlib.sha256(self.sock.getpeercert(binary_form=True)).hexdigest()
        if self.pins is not None and fp not in self.pins:
            self.close()
            raise CertificateChanged(self.host, fp)


class PinnedTransport(xmlrpc.client.SafeTransport):
    def __init__(self, pins, timeout=600):
        xmlrpc.client.SafeTransport.__init__(self)
        self.pins = pins
        self.timeout = timeout

    def make_connection(self, host):
        if self._connection and host == self._connection[0]:
            return self._connection[1]
        chost, self._extra_headers, _ = self.get_host_info(host)
        self._connection = host, PinnedConnection(chost, self.pins, self.timeout)
        return self._connection[1]


class Pool:
    """A logged-in session with the pool, found from any of its servers' addresses (a member
    sends it on to the coordinator)."""

    def __init__(self, conf, secrets, pins=None):
        p = conf.get("pool") or {}
        self.addresses = list(p.get("hosts") or [])
        self.user = p.get("user") or "root"
        self.password = secrets.get("pool_password", "")
        self.pins = set(pins) if pins is not None else set(p.get("pins") or [])
        self.session = None
        self.address = None

    def connect(self):
        import XenAPI
        errors = []
        queue = list(self.addresses)
        tried = set()
        while queue:
            a = queue.pop(0)
            if a in tried:
                continue
            tried.add(a)
            s = XenAPI.Session("https://" + bracket(a), transport=PinnedTransport(self.pins))
            try:
                s.xenapi.login_with_password(self.user, self.password, "1.0", "svm-helper/" + VERSION)
            except XenAPI.Failure as f:
                d = list(f.details or [])
                if d and d[0] == "HOST_IS_SLAVE" and len(d) > 1:
                    queue.insert(0, d[1])
                    continue
                if d and d[0] == "SESSION_AUTHENTICATION_FAILED":
                    raise PoolError("the pool refused the user name or password")
                errors.append("%s: %s" % (a, f))
                continue
            except CertificateChanged:
                raise
            except (OSError, xmlrpc.client.Error, http.client.HTTPException) as e:
                errors.append("%s: %s" % (a, short_error(e)))
                continue
            self.session, self.address = s, a
            return self
        raise PoolError("no server of the pool answered (%s)" % "; ".join(errors or ["no addresses"]))

    @property
    def x(self):
        return self.session.xenapi

    @property
    def sid(self):
        return self.session._session

    def close(self):
        if self.session is not None:
            try:
                self.session.xenapi.session.logout()
            except Exception:
                pass
        self.session = None

    def get(self, path, query, address=None, timeout=60):
        """A small HTTP GET of `path` from the coordinator (or `address`): its body."""
        conn = PinnedConnection(bracket(address or self.address), self.pins, timeout=timeout)
        try:
            conn.request("GET", path + "?" + urllib.parse.urlencode(dict(query, session_id=self.sid)), headers={"User-Agent": "svm-helper/" + VERSION})
            r = conn.getresponse()
            body = r.read()
            if r.status != 200:
                raise PoolError("the server said %d %s" % (r.status, r.reason))
            return body
        finally:
            conn.close()

    def upload(self, path, query, src, size, address=None, quiet=900):
        """An HTTP PUT of `size` bytes read from `src` (anything with .read(n)) to `path` on the
        coordinator (or `address`), in 1 MiB pieces (an import's task, if any, is in `query`)."""
        q = dict(query, session_id=self.sid)
        conn = PinnedConnection(bracket(address or self.address), self.pins, timeout=quiet)
        try:
            conn.putrequest("PUT", path + "?" + urllib.parse.urlencode(q))
            conn.putheader("Content-Length", str(size))
            conn.putheader("User-Agent", "svm-helper/" + VERSION)
            conn.endheaders()
            left = size
            while left > 0:
                chunk = src.read(min(1 << 20, left))
                if not chunk:
                    raise PoolError("the source stopped after %d of %d bytes" % (size - left, size))
                conn.send(chunk)
                left -= len(chunk)
            r = conn.getresponse()
            r.read()
            if r.status != 200:
                raise PoolError("the pool said %d %s" % (r.status, r.reason))
            return size
        finally:
            conn.close()


def read_settings(x, pool_ref):
    """What the app's Helper tab sets: where alerts go, and the rest (JSON in the pool)."""
    try:
        v = json.loads(x.pool.get_other_config(pool_ref).get(SETTINGS_KEY, "") or "{}")
        return v if isinstance(v, dict) else {}
    except Exception:
        return {}


def write_other_config(x, cls, ref, key, value):
    api = getattr(x, cls)
    try:
        api.remove_from_other_config(ref, key)
    except Exception:
        pass
    api.add_to_other_config(ref, key, value)


def create_message(x, name, priority, cls, obj_uuid, body):
    try:
        x.message.create(name, str(priority), cls, obj_uuid, body)
        return True
    except Exception as e:
        log("couldn't raise the alert '%s': %s" % (body, e))
        return False


# ---------------------------------------------------------------- sending alerts

def post(url, payload, headers=None, timeout=15):
    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, method="POST")
    req.add_header("Content-Type", "application/json")
    req.add_header("User-Agent", "svm-helper/" + VERSION)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status


def get_json(url, timeout=15):
    req = urllib.request.Request(url, headers={"User-Agent": "svm-helper/" + VERSION})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode("utf-8"))


def sends(settings):
    return any((c or {}).get("on") for c in (settings.get("channels") or {}).values())


def send(settings, title, text, priority, pool_name, extra=None):
    """Sends one alert to every channel that's on; what each said back ("ok" or why not)."""
    channels = settings.get("channels", {}) or {}
    results = {}
    mark = PRIORITY_MARK.get(priority, "🔵")
    payload = {"title": title, "message": text, "severity": PRIORITY_WORDS.get(priority, "Info").lower(), "priority": priority, "pool": pool_name, "source": "svm-helper"}
    payload.update(extra or {})

    def attempt(name, fn):
        try:
            fn()
            results[name] = "ok"
        except Exception as e:
            results[name] = short_error(e)
            log("couldn't send to %s: %s" % (name, short_error(e)))

    def body(head):
        b = "%s\n%s\n— %s" % (head, text, pool_name) if text else "%s\n— %s" % (head, pool_name)
        return b if len(b) <= 3900 else b[:3890] + "…"

    t = channels.get("telegram") or {}
    if t.get("on") and t.get("token") and t.get("chat"):
        attempt("telegram", lambda: post("https://api.telegram.org/bot%s/sendMessage" % t["token"].strip(), {"chat_id": t["chat"].strip(), "text": body(mark + " " + title), "disable_web_page_preview": True}))
    n = channels.get("ntfy") or {}
    if n.get("on") and n.get("url"):
        u = urllib.parse.urlsplit(n["url"].strip())
        level = {1: 5, 2: 4, 3: 4, 4: 3, 5: 2}.get(priority, 3)
        headers = {"Authorization": "Bearer " + n["token"].strip()} if n.get("token") else {}
        msg = "%s\n— %s" % (text, pool_name) if text else pool_name
        attempt("ntfy", lambda: post("%s://%s" % (u.scheme or "https", u.netloc), {"topic": u.path.strip("/"), "title": title, "message": msg[:3900], "priority": level}, headers))
    d = channels.get("discord") or {}
    if d.get("on") and d.get("url"):
        attempt("discord", lambda: post(d["url"].strip(), {"content": body("%s **%s**" % (mark, title))[:1990]}))
    h = channels.get("homeassistant") or {}
    if h.get("on") and h.get("url"):
        attempt("homeassistant", lambda: post(h["url"].strip(), payload))
    w = channels.get("webhook") or {}
    if w.get("on") and w.get("url"):
        attempt("webhook", lambda: post(w["url"].strip(), dict(payload, time=int(time.time()))))
    return results


def send_long(settings, title, text, pool_name):
    """A long text (the weekly review) in pieces short enough for every channel."""
    pieces, cur = [], ""
    for para in text.split("\n"):
        if len(cur) + len(para) + 1 > 3500 and cur:
            pieces.append(cur)
            cur = ""
        cur += para + "\n"
    if cur.strip():
        pieces.append(cur)
    results = {}
    for i, p in enumerate(pieces):
        head = title if len(pieces) == 1 else "%s (%d/%d)" % (title, i + 1, len(pieces))
        results = send(settings, head, p.strip(), 5, pool_name, {"name": "SVM_REVIEW"})
    return results


def in_quiet_hours(settings, now):
    q = settings.get("quiet") or {}
    if not q.get("on"):
        return False
    try:
        start = [int(p) for p in q.get("from", "23:00").split(":")]
        end = [int(p) for p in q.get("to", "07:00").split(":")]
    except Exception:
        return False
    lt = time.localtime(now)
    m, a, b = lt.tm_hour * 60 + lt.tm_min, start[0] * 60 + start[1], end[0] * 60 + end[1]
    return (a <= m < b) if a <= b else (m >= a or m < b)


# ---------------------------------------------------------------- alerts in words

VARIABLES = {
    "cpu_usage": "CPU",
    "memory_free_kib": "free memory",
    "network_usage": "network",
    "disk_usage": "disks",
    "physical_utilisation": "how full it is",
}


def object_name(x, cls, uuid):
    try:
        api = {"VM": x.VM, "Host": x.host, "SR": x.SR, "VMSS": x.VMSS, "Pool": x.pool}.get(cls)
        if api is not None:
            return api.get_name_label(api.get_by_uuid(uuid))
    except Exception:
        pass
    return ""


def describe(name, body, obj):
    """An alert's title and text, in words, from what the server said."""
    it = obj or "The pool"
    body = (body or "").strip()
    if name.startswith("SVM_"):
        return body, ""
    if name == "ALARM":
        var, value = "", ""
        for line in body.splitlines():
            if line.startswith("value:"):
                value = line.split(":", 1)[1].strip()
            if 'name value="' in line:
                var = line.split('name value="', 1)[1].split('"', 1)[0]
        what = VARIABLES.get(var, "storage traffic" if var.startswith("sr_io_throughput") else var or "a limit")
        try:
            v = float(value)
            figure = "%.0f%%" % (v * 100) if var in ("cpu_usage", "physical_utilisation") else value
        except Exception:
            figure = value
        return "%s: %s passed your limit" % (it, what), ("now %s" % figure) if figure else ""
    if name == "VMSS_SNAPSHOT_FAILED":
        detail = body.split("Details:", 1)[-1].strip() if "Details:" in body else body
        return "Snapshot schedule '%s' couldn't take its snapshots" % (obj or "?"), detail[:300]
    if name == "VMSS_SNAPSHOT_SUCCEEDED":
        return "Snapshot schedule '%s' took its snapshots" % (obj or "?"), ""
    if name.startswith("HOST_SERVER_CERTIFICATE_EXPIRING") or name.startswith("HOST_INTERNAL_CERTIFICATE_EXPIRING"):
        days = name.rsplit("_", 1)[-1]
        return "%s's certificate expires in %s days" % (it, days.lstrip("0") or "0"), ""
    if name.endswith("CERTIFICATE_EXPIRED"):
        return "%s's certificate has expired" % it, ""
    if name == "PBD_PLUG_FAILED_ON_SERVER_START":
        return "%s couldn't mount a storage when it started" % it, ""
    if name == "VM_CRASHED":
        return "%s crashed" % (obj or "A VM"), ""
    if name == "HOST_CLOCK_SKEW_DETECTED":
        return "%s's clock differs from the coordinator's" % it, ""
    if name in ("LICENSE_EXPIRES_SOON", "LICENSE_EXPIRED"):
        return "%s's license %s" % (it, "runs out soon" if name.endswith("SOON") else "has run out"), ""
    words = name.replace("_", " ").lower()
    return ("%s: %s" % (obj, words)) if obj else words.capitalize(), body[:300]


# ---------------------------------------------------------------- Claude

def claude_bin():
    """Claude Code, as installed for the helper (or anywhere on the PATH); "" when it isn't."""
    mine = os.path.join(CLAUDE_HOME, ".local", "bin", "claude")
    if os.access(mine, os.X_OK):
        return mine
    return shutil.which("claude") or ""


class Claude:
    """Claude, for alerts explained and the weekly review written. Two ways to reach it, as chosen
    at setup: the owner's Claude subscription, through Claude Code in print mode with the token
    'claude setup-token' gave (no extra cost: it's the plan's own use); or an Anthropic API key,
    billed per use. Each answer is plain text, or None (the reason in the log, and last_error)."""

    def __init__(self, conf, secrets):
        c = conf.get("claude") or {}
        self.key = secrets.get("anthropic_api_key", "")
        self.token = secrets.get("claude_oauth_token", "")
        self.mode = c.get("auth") or ("subscription" if self.token else "api" if self.key else "off")
        self.model = c.get("model") or MODEL
        self.cli_model = c.get("cli_model") or ""
        self.language = LANGUAGES.get(conf.get("language") or "en", conf.get("language") or "English")
        self.last_error = ""

    def ready(self):
        if self.mode == "subscription":
            return bool(self.token and claude_bin())
        return self.mode == "api" and bool(self.key)

    def how(self):
        return {"subscription": "your Claude subscription", "api": "an API key (%s)" % self.model}.get(self.mode, "off")

    def client(self):
        import anthropic
        return anthropic.Anthropic(api_key=self.key, timeout=300.0, max_retries=2)

    def check(self):
        """Whether Claude answers this way: raises when not. (An API key is checked without
        spending tokens; the subscription with a one-word question.)"""
        if self.mode == "subscription":
            if not self.write_cli("Answer with the single word: ok", "ok?", "low"):
                raise PoolError(self.last_error or "Claude Code didn't answer")
            return
        self.client().models.retrieve(self.model)

    def fail(self, why):
        self.last_error = why
        log("Claude: " + why)
        return None

    def write_cli(self, system, prompt, effort):
        """One answer from Claude Code in print mode, on the subscription: no tools, no session
        kept, a home of its own for the run (the prompt goes in on stdin)."""
        binary = claude_bin()
        if not binary:
            return self.fail("Claude Code isn't installed for the helper (sudo svm-helper setup)")
        home = tempfile.mkdtemp(prefix="svm-claude-")
        env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": home, "LANG": "C.UTF-8", "CLAUDE_CODE_OAUTH_TOKEN": self.token, "DISABLE_AUTOUPDATER": "1"}
        args = [binary, "-p", "--output-format", "json", "--tools", "", "--no-session-persistence", "--strict-mcp-config", "--disable-slash-commands", "--system-prompt", system, "--effort", effort]
        if self.cli_model:
            args += ["--model", self.cli_model]
        try:
            r = subprocess.run(args, input=prompt, capture_output=True, text=True, timeout=900, env=env, cwd=home)
        except subprocess.TimeoutExpired:
            return self.fail("Claude Code took too long")
        except OSError as e:
            return self.fail("couldn't run Claude Code: %s" % e)
        finally:
            shutil.rmtree(home, ignore_errors=True)
        out = r.stdout.strip()
        try:
            j = json.loads(out)
        except ValueError:
            if r.returncode != 0:
                return self.fail("Claude Code said: %s" % ((r.stderr or out).strip()[-300:] or "exit %d" % r.returncode))
            return out or self.fail("Claude Code gave no answer")
        if j.get("is_error"):
            return self.fail("Claude Code said: %s" % str(j.get("result") or j.get("subtype") or "an error")[:300])
        return (j.get("result") or "").strip() or self.fail("Claude Code gave no answer")

    def write(self, system, prompt, effort="medium", max_tokens=16000):
        if self.mode == "subscription":
            return self.write_cli(system, prompt, effort)
        import anthropic
        client = self.client()
        args = dict(model=self.model, max_tokens=max_tokens, output_config={"effort": effort}, system=system, messages=[{"role": "user", "content": prompt}])
        try:
            try:
                # if Claude declines, the API itself tries the request on a fallback model
                r = client.beta.messages.create(betas=["server-side-fallback-2026-07-01"], fallbacks="default", **args)
            except (TypeError, anthropic.BadRequestError):
                r = client.messages.create(**args)  # (an SDK or account without fallbacks)
        except anthropic.AuthenticationError:
            log("Claude: the API key was refused (sudo svm-helper setup to give another)")
            return None
        except anthropic.PermissionDeniedError:
            log("Claude: the API key can't use %s" % self.model)
            return None
        except anthropic.RateLimitError:
            log("Claude: too many requests just now, or the account is out of credit")
            return None
        except anthropic.APIStatusError as e:
            log("Claude: the API said %s: %s" % (e.status_code, short_error(e)))
            return None
        except anthropic.APIConnectionError:
            log("Claude: couldn't reach the API")
            return None
        if r.stop_reason == "refusal":
            log("Claude declined to answer this one")
            return None
        text = "".join(b.text for b in r.content if getattr(b, "type", "") == "text").strip()
        return text or None

    def explain(self, title, text, name, facts):
        system = (
            "You help the owner of a small XenServer pool at home understand its alerts, which reach "
            "them on the phone. You get one alert and a few facts about the pool. Answer in %s with "
            "two or three short sentences of plain text (no markdown, no lists): what it means in "
            "everyday words, and the first thing worth checking. If it needs nothing, say so. Don't "
            "invent anything the facts don't show." % self.language
        )
        prompt = "Alert: %s\n%s\n(XenServer's name for it: %s)\n\nThe pool now:\n%s" % (title, text, name, json.dumps(facts, ensure_ascii=False))
        return self.write(system, prompt, effort="low", max_tokens=4000)

    def review(self, snapshot):
        system = (
            "You look after a small XenServer pool at someone's home, and write its weekly health "
            "review for the owner, who reads it on the phone. You get a JSON snapshot of the pool "
            "as it is now and how its week went. Write in %s, as plain text (no markdown; a few "
            "emoji as bullets are fine). Start with a one-line verdict. Then only what needs "
            "attention, most important first, each with why it matters and what to do about it. "
            "End with one short line on what is fine. Keep it under 250 words. Use only what the "
            "snapshot shows; leave out what it doesn't say." % self.language
        )
        prompt = "The pool's snapshot:\n" + json.dumps(snapshot, ensure_ascii=False, indent=1)
        return self.write(system, prompt, effort="medium", max_tokens=16000)

    def advise(self, checks, snapshot):
        """Its words for the Advisor's checklist: JSON, as text (see worded)."""
        system = (
            "You are an experienced XenServer administrator checking a small pool at someone's home "
            "against good practice, for the Advisor page of Simple VM Manager, the app they manage it "
            "with. You get the checks the pool's helper made (JSON: an id, a topic, a status of ok, "
            "warn or bad, an English title and note, and the objects involved) and a detailed "
            "snapshot of the pool: its servers, storage, networks and every VM (its system, VM Tools, "
            "disks and where they are, ISO in its drive, snapshots, schedule, and its processors' and "
            "memory's use over the last week). Write in %s, in plain words for someone who knows "
            "their pool but not every XenServer term.\n\n"
            "1. For every check: a short title (under 70 characters; keep the names of servers, VMs "
            "and storage as they are) and, when it isn't ok, a note of one to three sentences: why it "
            "matters here, and what to do. Keep each status as the helper set it.\n"
            "2. Then look through the snapshot yourself, VM by VM and server by server, and add five "
            "to ten findings of your own that the checks don't cover. Be specific: name the VM or "
            "server and give the figures. Good ones: a VM with far more or less memory or vCPUs than "
            "its week's use calls for; a system near or past its makers' support, or VM Tools much "
            "older than the others; a VM whose disks sit on storage that's nearly full, or on a "
            "different server's storage than where it runs; a server much busier than another; "
            "important VMs (by their names) without a schedule or not starting by themselves; old "
            "or unused things that take room; a XenServer version near the end of its support. "
            "Include what you checked and found right too (status ok), so the owner sees it was "
            "looked at. Each has a topic (backups, storage, servers, vms or security), a status, a "
            "title and a note (none for ok). Don't repeat the helper's checks, and don't invent what "
            "the JSON doesn't show.\n\n"
            "Answer with JSON only, no markdown: "
            '{"checks": {"<id>": {"title": "...", "note": "..."}}, "more": [{"topic": "...", "status": "...", "title": "...", "note": "..."}]}' % self.language
        )
        prompt = "The checks:\n%s\n\nThe pool's snapshot:\n%s" % (json.dumps(checks, ensure_ascii=False, indent=1), json.dumps(snapshot, ensure_ascii=False))
        return self.write(system, prompt, effort="high", max_tokens=16000)


# ---------------------------------------------------------------- what it does each minute

class Look:
    """What one look (a minute's work) needs: the session, the pool's records, the settings."""

    def __init__(self, pool, conf, secrets, state, now):
        self.pool = pool
        self.x = pool.x
        self.conf = conf
        self.secrets = secrets
        self.state = state
        self.now = now
        self.pool_ref = self.x.pool.get_all()[0]
        self.pool_rec = self.x.pool.get_record(self.pool_ref)
        self.pool_name = self.pool_rec.get("name_label") or "pool"
        self.pool_uuid = self.pool_rec.get("uuid", "")
        self.master = self.pool_rec.get("master")
        self.hosts = self.x.host.get_all_records()
        self.settings = read_settings(self.x, self.pool_ref)
        self.claude = Claude(conf, secrets)
        state["settings"] = self.settings
        state["pool_name"] = self.pool_name
        state["hosts_seen"] = {h.get("address", ""): h.get("name_label", "") for h in self.hosts.values() if h.get("address")}

    def save(self):
        try:
            save_json(state_path("state"), self.state)
        except OSError as e:
            log("couldn't save its state: %s" % e)

    def alert(self, name, priority, cls, uuid, text, event=True):
        """An alert in the pool (it shows in the app and XenCenter, and goes to the phone with the
        others); kept in this week's events too."""
        create_message(self.x, name, priority, cls, uuid, text)
        if event:
            add_event(self.state, self.now, text)
        log(text)
        return text

    def host_name(self, ref):
        return (self.hosts.get(ref) or {}).get("name_label", "?")


def add_event(state, now, text):
    ev = state.setdefault("events", [])
    ev.append({"t": int(now), "text": text})
    state["events"] = [e for e in ev if now - e.get("t", 0) < 14 * 86400][-300:]


def pool_offset(look):
    """The coordinator's local time minus UTC, in seconds (its snapshot schedules run on it)."""
    st = look.state
    if look.now - st.get("offset_at", 0) < 6 * 3600 and "offset" in st:
        return st["offset"]
    try:
        local = xen_epoch(look.x.host.get_server_localtime(look.master))
        utc = xen_epoch(look.x.host.get_servertime(look.master))
        off = int(round((local - utc) / 900.0) * 900) if local and utc else 0
    except Exception:
        off = st.get("offset", 0)
    st["offset"], st["offset_at"] = off, look.now
    return off


def last_expected(frequency, schedule, now, offset=0):
    """When a snapshot schedule should last have run, on the pool's clock (`offset` seconds from
    UTC), as XenServer's vmss plugin works it out; seconds since 1970, None when unreadable."""
    try:
        minute = int(schedule.get("min", "0"))
        t = datetime.datetime.fromtimestamp(now + offset, datetime.timezone.utc).replace(tzinfo=None)

        def epoch(dt):
            return calendar.timegm(dt.timetuple()) - offset

        if frequency == "hourly":
            cand = t.replace(minute=minute, second=0, microsecond=0)
            if cand > t:
                cand -= datetime.timedelta(hours=1)
            return epoch(cand)
        hour = int(schedule.get("hour", "0"))
        cand = t.replace(hour=hour, minute=minute, second=0, microsecond=0)
        if frequency == "daily":
            if cand > t:
                cand -= datetime.timedelta(days=1)
            return epoch(cand)
        if frequency == "weekly":
            days = set(d.strip() for d in schedule.get("days", "").split(",") if d.strip())
            for back in range(0, 8):
                c = cand - datetime.timedelta(days=back)
                if c <= t and c.strftime("%A") in days:
                    return epoch(c)
    except Exception:
        pass
    return None


def remount(look):
    """Shared storage that a server had mounted before it last started, and couldn't mount when
    it did (it started before the NAS): mounted again, every two minutes for a day."""
    done = []
    if not on(look.settings, "remount", default=True):
        return done
    x, now = look.x, look.now
    seen = look.state.setdefault("attached", {})
    tries = look.state.setdefault("remount_try", {})
    srs = x.SR.get_all_records()
    for ref, p in x.PBD.get_all_records().items():
        uuid = p["uuid"]
        sr = srs.get(p["SR"])
        if not sr or not sr.get("shared"):
            continue
        if p["currently_attached"]:
            seen[uuid] = now
            continue
        host = look.hosts.get(p["host"])
        if not host or not host.get("enabled"):
            continue
        try:
            booted = float((host.get("other_config") or {}).get("boot_time", "0"))
        except ValueError:
            booted = 0
        last = seen.get(uuid)
        if not last or not booted or last > booted or now - booted > 86400:
            continue
        if now - tries.get(uuid, 0) < 120:
            continue
        tries[uuid] = now
        try:
            x.PBD.plug(ref)
            seen[uuid] = now
            done.append(look.alert("SVM_STORAGE_MOUNTED", 5, "SR", sr["uuid"], "Mounted '%s' again on %s" % (sr["name_label"], host["name_label"])))
        except Exception as e:
            log("couldn't mount '%s' on %s yet: %s" % (sr["name_label"], host["name_label"], short_error(e)))
    return done


def storage(look):
    """Storage fuller than its limit: the one set on it (its Alerts), else the helper's."""
    done = []
    if not on(look.settings, "watch", "storage", "on", default=True):
        return done
    x, now = look.x, look.now
    try:
        level = float(((look.settings.get("watch") or {}).get("storage") or {}).get("level", 90)) / 100.0
    except (TypeError, ValueError):
        level = 0.9
    marks = look.state.setdefault("full", {})
    stats = set()
    try:
        for sm in x.SM.get_all_records().values():
            caps = list(sm.get("capabilities") or []) + list((sm.get("features") or {}).keys())
            if "SR_STATS" in caps:
                stats.add(sm.get("type"))
    except Exception:
        pass
    pbds = x.PBD.get_all_records()
    for ref, sr in x.SR.get_all_records().items():
        size, used = int(sr.get("physical_size", 0) or 0), int(sr.get("physical_utilisation", 0) or 0)
        if size <= 0 or used < 0 or sr.get("type") in ("udev", "iso") or sr.get("content_type") == "iso" or sr.get("type") in stats:
            continue
        limit = level
        perfmon = (sr.get("other_config") or {}).get("perfmon", "")
        if 'name value="physical_utilisation"' in perfmon and "alarm_trigger_level" in perfmon:
            try:
                limit = float(perfmon.split('physical_utilisation"', 1)[1].split('alarm_trigger_level value="', 1)[1].split('"', 1)[0])
            except (IndexError, ValueError):
                pass
        full = float(used) / size
        uuid = sr["uuid"]
        if full > limit:
            if now - marks.get(uuid, 0) < 86400:
                continue
            marks[uuid] = now
            name = sr["name_label"]
            if not sr.get("shared"):
                owners = [look.host_name(p["host"]) for p in pbds.values() if p["SR"] == ref and p["host"] in look.hosts]
                if owners:
                    name = "%s on %s" % (name, owners[0])
            done.append(look.alert("SVM_STORAGE_FULL", 3, "SR", uuid, "%s is %.0f%% full (your limit: %.0f%%); %s left" % (name, full * 100, limit * 100, gb(size - used))))
        elif full < limit - 0.02:
            marks.pop(uuid, None)
    return done


def schedules(look):
    """A snapshot schedule's time that went by without a run (the server off then, or the
    schedule changed after it): its run made up, once, within a day."""
    done = []
    if not on(look.settings, "watch", "schedules", default=True):
        return done
    x, now = look.x, look.now
    made = look.state.setdefault("madeup", {})
    try:
        all_vmss = x.VMSS.get_all_records()
    except Exception:
        return done
    offset = pool_offset(look)
    for ref, v in all_vmss.items():
        if not v.get("enabled") or not v.get("VMs"):
            continue
        last_run = xen_epoch(v.get("last_run_time"))
        if last_run <= 0:
            continue  # never run: XenServer's plugin runs it at its first look
        expected = last_expected(v.get("frequency", ""), v.get("schedule") or {}, now, offset)
        if expected is None or last_run >= expected - 60:
            continue
        if now - expected < 16 * 60 or now - expected > 86400:
            continue  # its own look (every 15 minutes) comes first; a day later, it's let go
        if made.get(v["uuid"]) == expected:
            continue
        made[v["uuid"]] = expected
        look.save()
        try:
            said = x.host.call_plugin(look.master, "vmss", "snapshot_now", {"vmss_uuid": v["uuid"]})
        except Exception as e:
            said = short_error(e)
        text = "Made up the missed %s run of '%s'" % (time.strftime("%H:%M", time.gmtime(expected + offset)), v["name_label"])
        if said != "True":
            text += " (it said: %s)" % said
        done.append(look.alert("SVM_SCHEDULE_MADE_UP", 5, "VMSS", v["uuid"], text))
    return done


def facts_for_claude(look):
    """A few facts about the pool, for Claude to explain an alert with."""
    try:
        hm = look.x.host_metrics.get_all_records()
    except Exception:
        hm = {}
    hosts = []
    for h in look.hosts.values():
        m = hm.get(h.get("metrics"), {})
        hosts.append({"name": h.get("name_label"), "on": bool(m.get("live")), "coordinator": h.get("uuid") == (look.hosts.get(look.master) or {}).get("uuid")})
    return {"pool": look.pool_name, "servers": hosts, "product": (look.hosts.get(look.master) or {}).get("software_version", {}).get("product_brand", "XenServer")}


def forward(look):
    """The pool's new alerts, as bad as asked or worse, to the channels that are on."""
    done = []
    st, now, x = look.state, look.now, look.x
    since = st.get("fwd_since")
    if since is None:
        st["fwd_since"] = now  # (from now on: nothing old is sent)
        return done
    if not sends(look.settings):
        st["fwd_since"] = now
        return done
    try:
        worst = int(look.settings.get("min_priority", 3))
    except (TypeError, ValueError):
        worst = 3
    try:
        msgs = x.message.get_since(xen_date(since))
    except Exception as e:
        log("couldn't read the pool's alerts: %s" % short_error(e))
        return done
    quiet = in_quiet_hours(look.settings, now)
    c = look.conf.get("claude") or {}
    explain = look.claude.ready() and c.get("explain", True)
    day = time.strftime("%Y-%m-%d")
    if st.get("explained_day") != day:
        st["explained_day"], st["explained"] = day, 0
    newest, sent, held = since, 0, 0
    for m in sorted(msgs.values(), key=lambda m: xen_epoch(m.get("timestamp"))):
        t = xen_epoch(m.get("timestamp"))
        newest = max(newest, t)
        if t <= since:
            continue
        try:
            priority = int(m.get("priority", 5))
        except (TypeError, ValueError):
            priority = 5
        if priority > worst or (quiet and priority > 1):
            continue
        if sent >= 10:
            held += 1
            continue
        name = m.get("name", "")
        obj = object_name(x, m.get("cls", ""), m.get("obj_uuid", ""))
        title, text = describe(name, m.get("body", ""), obj)
        if explain and priority <= int(c.get("explain_priority", 2)) and st["explained"] < int(c.get("explain_per_day", 10)):
            st["explained"] += 1
            words = look.claude.explain(title, text, name, facts_for_claude(look))
            if words:
                text = (text + "\n\n" if text else "") + "💡 " + words
        send(look.settings, title, text, priority, look.pool_name, {"name": name, "object": obj})
        sent += 1
    if held:
        send(look.settings, "%d more alerts" % held, "See them in Simple VM Manager or XenCenter.", 3, look.pool_name)
    st["fwd_since"] = newest
    if sent:
        done.append("sent %d alert%s" % (sent, "" if sent == 1 else "s"))
    return done


def hosts_watch(look):
    """Each server knocked on (its HTTPS port): one that stops answering for two minutes is
    said so, and so is its coming back."""
    done = []
    if not on(look.conf, "watch", "hosts", default=True):
        return done
    reach = look.state.setdefault("reach", {})
    for ref, h in look.hosts.items():
        address, uuid, name = h.get("address", ""), h.get("uuid", ""), h.get("name_label", "?")
        if not address:
            continue
        r = reach.setdefault(uuid, {"fails": 0})
        if tcp_ok(address, 443, 3):
            if r.get("down_since"):
                done.append(look.alert("SVM_HOST_BACK", 3, "Host", uuid, "%s (%s) answers again, after %s" % (name, address, span(look.now - r["down_since"]))))
            reach[uuid] = {"fails": 0}
            continue
        r["fails"] = r.get("fails", 0) + 1
        if r["fails"] == 2:
            r["down_since"] = look.now - 60
            done.append(look.alert("SVM_HOST_UNREACHABLE", 2, "Host", uuid, "%s (%s) isn't answering" % (name, address)))
    return done


def clocks(look):
    """Each hour: each server's clock against the right time (this machine's, kept by NTP)."""
    done = []
    st = look.state
    if not on(look.conf, "watch", "clocks", default=True) or look.now - st.get("clocks_at", 0) < 3600:
        return done
    st["clocks_at"] = look.now
    if ntp_synced() is False:
        log("this machine's clock isn't kept by NTP: the servers' clocks aren't compared")
        return done
    marks = st.setdefault("clock_marks", {})
    drift = st.setdefault("clock", {})
    for ref, h in look.hosts.items():
        try:
            t0 = time.time()
            there = xen_epoch(look.x.host.get_servertime(ref))
            here = (t0 + time.time()) / 2
        except Exception:
            continue
        if not there:
            continue
        d = there - here
        drift[h["uuid"]] = round(d)
        if abs(d) > 60:
            if look.now - marks.get(h["uuid"], 0) > 86400:
                marks[h["uuid"]] = look.now
                text = "%s's clock is %s %s the right time" % (h["name_label"], span(abs(d)), "ahead of" if d > 0 else "behind")
                done.append(look.alert("SVM_CLOCK_OFF", 3, "Host", h["uuid"], text + " (is NTP set on it?)"))
        else:
            marks.pop(h["uuid"], None)
    return done


def probe(url, timeout=8):
    """Whether a service answers: (yes/no, why not). http(s)://... answers with any reply under
    500; tcp://host:port when its port takes a connection."""
    u = urllib.parse.urlsplit(url)
    try:
        if u.scheme == "tcp":
            with socket.create_connection((u.hostname, u.port or 80), timeout=timeout):
                return True, ""
        req = urllib.request.Request(url, headers={"User-Agent": "svm-helper/" + VERSION})
        try:
            with urllib.request.urlopen(req, timeout=timeout, context=unverified_context() if u.scheme == "https" else None) as r:
                return r.status < 500, "it said %d" % r.status
        except urllib.error.HTTPError as e:
            return e.code < 500, "it said %d" % e.code
    except Exception as e:
        return False, short_error(getattr(e, "reason", e))


def services(look):
    """The services named in the settings (Home Assistant, the NAS...): down three looks in a
    row, said so; back, said so."""
    done = []
    items = (look.conf.get("watch") or {}).get("services") or []
    st = look.state.setdefault("services", {})
    for s in items:
        name, url = s.get("name") or s.get("url", "?"), s.get("url", "")
        if not url:
            continue
        r = st.setdefault(url, {"fails": 0})
        ok, why = probe(url)
        if ok:
            if r.get("down_since"):
                done.append(look.alert("SVM_SERVICE_BACK", 3, "Pool", look.pool_uuid, "%s answers again, after %s" % (name, span(look.now - r["down_since"]))))
            st[url] = {"fails": 0}
            continue
        r["fails"] = r.get("fails", 0) + 1
        if r["fails"] == 3:
            r["down_since"] = look.now - 120
            done.append(look.alert("SVM_SERVICE_DOWN", 3, "Pool", look.pool_uuid, "%s isn't answering (%s: %s)" % (name, url, why)))
    st_keys = set(s.get("url") for s in items)
    for k in list(st):
        if k not in st_keys:
            st.pop(k)
    return done


def heartbeat(look, started, force=False):
    """Every five minutes: this helper's news in the pool, for Simple VM Manager to show."""
    st = look.state
    adv = load_json(state_path("advice"), {})
    test = load_json(state_path("test"), {})
    fresh = max(adv.get("at", 0), test.get("at", 0), load_json(state_path("update"), {}).get("at", 0)) > st.get("beat_at", 0) or st.get("beat_version") != VERSION
    if not force and not fresh and look.now - st.get("beat_at", 0) < 300:
        return
    st["beat_at"], st["beat_version"] = look.now, VERSION
    r = load_json(state_path("review"), {})
    conf = look.conf
    news = {
        "version": VERSION,
        "machine": socket.gethostname(),
        "address": my_address(look.pool.address),
        "vm": conf.get("self_vm", ""),
        "since": int(started),
        "last_look": int(look.now),
        "claude": look.claude.ready(),
        "review": {"on": on(conf, "claude", "review", "on", default=True), "last": r.get("at", 0)},
        "watching": len((conf.get("watch") or {}).get("services") or []),
        "advice": {"on": on(conf, "claude", "advice", "on", default=True), "last": adv.get("at", 0), "request": adv.get("request", 0), "bad": adv.get("bad", 0), "warn": adv.get("warn", 0)},
        "copy": 1,
        "requests": 1,  # it answers the app's requests (REQUEST_KEY)
        "updates": 1 if can_update() else 0,  # it updates itself when asked
    }
    if test:
        news["test"] = test
    upd = load_json(state_path("update"), {})
    if upd:
        news["update"] = upd
    try:
        write_other_config(look.x, "pool", look.pool_ref, STATUS_KEY, json.dumps(news, sort_keys=True))
    except Exception as e:
        log("couldn't leave its heartbeat in the pool: %s" % short_error(e))


# ---------------------------------------------------------------- jobs that take a while

def start_in_order(pool, settings, boot):
    """Once after the pool starts: the VMs on the list, in order, a wait after each."""
    x = pool.x
    so = settings.get("start_order") or {}
    pool_ref = x.pool.get_all()[0]
    master = x.pool.get_master(pool_ref)
    for _ in range(60):  # the coordinator taking VMs (five minutes at most)
        try:
            if x.host.get_enabled(master):
                break
        except Exception:
            pass
        time.sleep(5)
    started, failed = [], []
    for item in so.get("vms", []):
        try:
            vm = x.VM.get_by_uuid(item.get("uuid", ""))
            name = x.VM.get_name_label(vm)
        except Exception:
            continue
        try:
            if x.VM.get_power_state(vm) == "Halted":
                x.VM.start(vm, False, False)
                started.append(name)
                log("started '%s' (in order)" % name)
                time.sleep(max(0, min(int(item.get("wait", 30)), 3600)))
        except Exception as e:
            failed.append("%s (%s)" % (name, short_error(e)))
            log("couldn't start '%s': %s" % (name, short_error(e)))
    uuid = x.pool.get_uuid(pool_ref)
    if started:
        create_message(x, "SVM_STARTED_IN_ORDER", 5, "Pool", uuid, "Started in order: %s" % ", ".join(started))
    if failed:
        create_message(x, "SVM_START_FAILED", 2, "Pool", uuid, "Couldn't start in order: %s" % "; ".join(failed))


# ---------------------------------------------------------------- the weekly review

def parse_rrd(body, out):
    """A server's /rrd_updates (XML: a legend, then a row an hour) into `out`: for each server and
    VM, its processors' and its memory's (average %, peak %) over those hours."""
    import xml.etree.ElementTree as ET
    root = ET.fromstring(body)
    legend = [e.text or "" for e in root.iter("entry")]
    rows = []
    for row in root.iter("row"):
        vals = []
        for v in row.findall("v"):
            try:
                vals.append(float(v.text))
            except (TypeError, ValueError):
                vals.append(float("nan"))
        rows.append(vals)
    cols = {}
    for i, name in enumerate(legend):
        parts = name.split(":")
        if len(parts) >= 4 and parts[1] in ("host", "vm"):
            cols.setdefault((parts[1], parts[2]), {})[":".join(parts[3:])] = i

    def at(r, i):
        v = r[i] if i < len(r) else float("nan")
        return v if v == v else None

    def summed(series):
        series = [v for v in series if v is not None]
        return (int(round(100 * sum(series) / len(series))), int(round(100 * max(series)))) if series else None

    for (kind, uuid), ds in cols.items():
        got = {}
        cpus = [i for d, i in ds.items() if re.fullmatch(r"cpu\d+", d)]
        if kind == "host" and "cpu_avg" in ds:
            got["cpu"] = summed(at(r, ds["cpu_avg"]) for r in rows)
        elif cpus:
            got["cpu"] = summed((sum(at(r, i) for i in cpus) / len(cpus)) if all(at(r, i) is not None for i in cpus) else None for r in rows)
        if kind == "host" and "memory_total_kib" in ds and "memory_free_kib" in ds:
            got["memory"] = summed((1 - at(r, ds["memory_free_kib"]) / at(r, ds["memory_total_kib"])) if at(r, ds["memory_total_kib"]) and at(r, ds["memory_free_kib"]) is not None else None for r in rows)
        elif kind == "vm" and "memory" in ds and "memory_internal_free" in ds:
            got["memory"] = summed((1 - at(r, ds["memory_internal_free"]) * 1024 / at(r, ds["memory"])) if at(r, ds["memory"]) and at(r, ds["memory_internal_free"]) is not None else None for r in rows)
        got = {k: v for k, v in got.items() if v}
        if got:
            out[kind][uuid] = got
    return out


def week_load(pool, now=None):
    """Each server's and each running VM's load over the last week, as the servers keep it (an
    average an hour): {"host"|"vm": {uuid: {"cpu"|"memory": (average %, peak %)}}}. A server
    that can't be read is left out."""
    now = now or time.time()
    out = {"host": {}, "vm": {}}
    for h in pool.x.host.get_all_records().values():
        if not h.get("address"):
            continue
        try:
            parse_rrd(pool.get("/rrd_updates", {"start": int(now - 7 * 86400), "cf": "AVERAGE", "interval": 3600, "host": "true"}, address=h["address"], timeout=30), out)
        except Exception as e:
            log("couldn't read %s's load of the week: %s" % (h.get("name_label"), short_error(e)))
    return out


def snapshot_of_pool(x, state, load=None):
    """The pool as it is, and how its week went (`load`: week_load's), for the review and the
    Advisor (no passwords or keys)."""
    now = time.time()
    load = load or {"host": {}, "vm": {}}
    pool_ref = x.pool.get_all()[0]
    pool = x.pool.get_record(pool_ref)
    hosts = x.host.get_all_records()
    hm = x.host_metrics.get_all_records()
    srs = x.SR.get_all_records()
    pbds = x.PBD.get_all_records()
    vms = x.VM.get_all_records()
    clock_drift = state.get("clock", {})
    out = {"pool": pool.get("name_label"), "written": time.strftime("%Y-%m-%d %H:%M"), "high_availability": pool.get("ha_enabled"), "default_storage": srs.get(pool.get("default_SR"), {}).get("name_label"), "servers": [], "storage": [], "vms": [], "networks": [], "schedules": [], "alerts_this_week": {}, "important_alerts": [], "helper_events": [], "unused_disks": [], "old_snapshots": []}

    def records(cls):
        try:
            return getattr(x, cls).get_all_records()
        except Exception:
            return {}

    vdis, vbds, vifs, nets, gms, vmms = records("VDI"), records("VBD"), records("VIF"), records("network"), records("VM_guest_metrics"), records("VM_metrics")
    vmss = records("VMSS")
    pct = lambda w, k: ({"average_pct": w[k][0], "peak_pct": w[k][1]} if w.get(k) else None)
    for ref, h in hosts.items():
        m = hm.get(h.get("metrics"), {})
        try:
            boot = float((h.get("other_config") or {}).get("boot_time", "0"))
        except ValueError:
            boot = 0
        total, free = int(m.get("memory_total", 0) or 0), int(m.get("memory_free", 0) or 0)
        out["servers"].append({
            "name": h.get("name_label"), "address": h.get("address"), "coordinator": ref == pool.get("master"),
            "on": bool(m.get("live")), "enabled": h.get("enabled"), "up_for": span(now - boot) if boot else None,
            "version": "%s %s" % (h.get("software_version", {}).get("product_brand", ""), h.get("software_version", {}).get("product_version", "")),
            "cpus": int((h.get("cpu_info") or {}).get("cpu_count", 0) or 0),
            "memory_used_pct": round(100 * (total - free) / total) if total else None, "memory_total": gb(total) if total else None,
            "clock_off_seconds": clock_drift.get(h.get("uuid")),
            "cpu_model": (h.get("cpu_info") or {}).get("modelname"),
            "updates_waiting": h.get("latest_synced_updates_applied") == "no" if "latest_synced_updates_applied" in h else None,
            "needs_restart_for_updates": bool(h.get("pending_guidances")),
            "in_maintenance": not h.get("enabled"),
            "running_vms": sum(1 for v in vms.values() if v.get("resident_on") == ref and v.get("power_state") == "Running" and not v.get("is_control_domain")),
            "this_week": {"processors": pct(load["host"].get(h.get("uuid"), {}), "cpu"), "memory": pct(load["host"].get(h.get("uuid"), {}), "memory")},
        })
    for ref, sr in srs.items():
        if sr.get("type") in ("udev",) or sr.get("content_type") == "iso":
            continue
        size, used = int(sr.get("physical_size", 0) or 0), int(sr.get("physical_utilisation", 0) or 0)
        if size <= 0:
            continue
        where = [hosts[p["host"]]["name_label"] for p in pbds.values() if p["SR"] == ref and p["host"] in hosts]
        out["storage"].append({"name": sr["name_label"], "type": sr.get("type"), "shared": sr.get("shared"), "size": gb(size), "used_pct": round(100.0 * used / size), "free": gb(size - used), "on": where if not sr.get("shared") else "all"})
    snaps_by_vm = {}
    for ref, v in vms.items():
        if v.get("is_a_snapshot"):
            snaps_by_vm.setdefault(v.get("snapshot_of"), []).append(v)
            age = (now - xen_epoch(v.get("snapshot_time"))) / 86400.0
            if age > 30 and not v.get("is_vmss_snapshot"):
                parent = vms.get(v.get("snapshot_of"), {}).get("name_label", "?")
                out["old_snapshots"].append({"vm": parent, "name": v.get("name_label"), "days_old": int(age)})
    for ref, v in vms.items():
        if v.get("is_a_template") or v.get("is_a_snapshot") or v.get("is_control_domain"):
            continue
        snaps = snaps_by_vm.get(ref, [])
        disks, iso, networks = [], None, []
        for b in v.get("VBDs") or []:
            vbd = vbds.get(b, {})
            d = vdis.get(vbd.get("VDI"), {})
            if vbd.get("type") == "CD":
                iso = d.get("name_label") if not vbd.get("empty") else None
            elif d:
                sr = srs.get(d.get("SR"), {})
                disks.append({"size": gb(int(d.get("virtual_size", 0) or 0)), "used": gb(int(d.get("physical_utilisation", 0) or 0)), "storage": sr.get("name_label"), "shared_storage": bool(sr.get("shared"))})
        for f in v.get("VIFs") or []:
            networks.append(nets.get(vifs.get(f, {}).get("network"), {}).get("name_label"))
        gm = gms.get(v.get("guest_metrics"), {})
        tools = gm.get("PV_drivers_version") or {}
        started = xen_epoch(vmms.get(v.get("metrics"), {}).get("start_time"))
        oldest = min((xen_epoch(x_.get("snapshot_time")) for x_ in snaps), default=0)
        w = load["vm"].get(v.get("uuid"), {})
        out["vms"].append({
            "name": v.get("name_label"), "description": (v.get("name_description") or "")[:120], "state": v.get("power_state"), "on_server": hosts.get(v.get("resident_on"), {}).get("name_label"),
            "starts_with_server": (v.get("other_config") or {}).get("auto_poweron") == "true",
            "memory": gb(int(v.get("memory_dynamic_max", 0) or 0)), "memory_min": gb(int(v.get("memory_dynamic_min", 0) or 0)), "vcpus": int(v.get("VCPUs_max", 0) or 0),
            "boot": "UEFI" if (v.get("HVM_boot_params") or {}).get("firmware") == "uefi" else "BIOS",
            "system": os_of(gm) or None, "vm_tools": ".".join(str(tools.get(k, "")) for k in ("major", "minor", "micro") if tools.get(k) is not None) or None,
            "disks": disks, "iso_in_drive": iso, "networks": networks,
            "running_for_days" if v.get("power_state") == "Running" else "last_started_days_ago": int((now - started) / 86400) if started else None,
            "snapshots": len(snaps), "oldest_snapshot_days": int((now - oldest) / 86400) if oldest else None,
            "snapshot_schedule": vmss.get(v.get("snapshot_schedule"), {}).get("name_label"),
            "this_week": {"processors": pct(w, "cpu"), "memory": pct(w, "memory")} if w else None,
        })
    for n in nets.values():
        if n.get("bridge", "").startswith("xapi") and not n.get("PIFs"):
            continue
        out["networks"].append({"name": n.get("name_label"), "mtu": n.get("MTU"), "vms": len(n.get("VIFs") or [])})
    try:
        for ref, s in x.VMSS.get_all_records().items():
            out["schedules"].append({"name": s.get("name_label"), "on": s.get("enabled"), "every": s.get("frequency"), "vms": len(s.get("VMs") or []), "keeps": s.get("retained_snapshots"), "last_ran": time.strftime("%Y-%m-%d %H:%M", time.gmtime(xen_epoch(s.get("last_run_time")))) if xen_epoch(s.get("last_run_time")) else None})
    except Exception:
        pass
    try:
        msgs = x.message.get_since(xen_date(now - 7 * 86400))
        counts = {}
        important = []
        for m in msgs.values():
            name = m.get("name", "")
            counts[name] = counts.get(name, 0) + 1
            if int(m.get("priority", 5)) <= 3:
                title, text = describe(name, m.get("body", ""), object_name(x, m.get("cls", ""), m.get("obj_uuid", "")))
                important.append((xen_epoch(m.get("timestamp")), title, text))
        out["alerts_this_week"] = counts
        out["important_alerts"] = [{"when": time.strftime("%a %H:%M", time.localtime(t)), "what": title, "detail": text[:200]} for t, title, text in sorted(important)[-25:]]
    except Exception:
        pass
    out["helper_events"] = [{"when": time.strftime("%a %H:%M", time.localtime(e["t"])), "what": e["text"]} for e in state.get("events", []) if now - e.get("t", 0) < 7 * 86400][-40:]
    try:
        vdis = x.VDI.get_all_records()
        for ref, d in vdis.items():
            sr = srs.get(d.get("SR"), {})
            if d.get("type") != "user" or d.get("is_a_snapshot") or d.get("VBDs") or not d.get("managed") or sr.get("content_type") == "iso" or d.get("is_tools_iso"):
                continue
            out["unused_disks"].append({"name": d.get("name_label"), "size": gb(int(d.get("virtual_size", 0) or 0)), "storage": sr.get("name_label")})
        out["unused_disks"] = out["unused_disks"][:20]
    except Exception:
        pass
    return out


def plain_review(snap):
    """The weekly review without Claude: the figures that matter, and what needs a look."""
    attention = []
    for s in snap["servers"]:
        if not s["on"]:
            attention.append("%s is off or not answering" % s["name"])
        if s.get("memory_used_pct") and s["memory_used_pct"] > 90:
            attention.append("%s uses %d%% of its memory" % (s["name"], s["memory_used_pct"]))
        if s.get("clock_off_seconds") and abs(s["clock_off_seconds"]) > 60:
            attention.append("%s's clock is %s off" % (s["name"], span(abs(s["clock_off_seconds"]))))
    for st in snap["storage"]:
        if st["used_pct"] >= 85:
            attention.append("%s is %d%% full (%s free)" % (st["name"], st["used_pct"], st["free"]))
    if snap["unused_disks"]:
        attention.append("%d disks aren't used by any VM" % len(snap["unused_disks"]))
    if snap["old_snapshots"]:
        attention.append("%d snapshots are over 30 days old" % len(snap["old_snapshots"]))
    running = sum(1 for v in snap["vms"] if v["state"] == "Running")
    lines = ["✅ All looks well." if not attention else "⚠️ %d thing%s to look at:" % (len(attention), "" if len(attention) == 1 else "s")]
    lines += ["• " + a for a in attention]
    lines.append("")
    lines.append("Servers: %d, %d on · VMs: %d, %d running" % (len(snap["servers"]), sum(1 for s in snap["servers"] if s["on"]), len(snap["vms"]), running))
    alerts = sum(snap["alerts_this_week"].values())
    lines.append("Alerts this week: %d (%d important)" % (alerts, len(snap["important_alerts"])))
    return "\n".join(lines)


def write_review(pool, conf, secrets, send_it=True):
    """The weekly review, by Claude (or plain without a key): sent to the channels, kept in
    review.json; its text."""
    x = pool.x
    state = load_json(state_path("state"), {})
    snap = snapshot_of_pool(x, state, week_load(pool))
    claude = Claude(conf, secrets)
    text = claude.review(snap) if claude.ready() else None
    by = "claude" if text else "plain"
    if not text:
        text = plain_review(snap)
    if send_it:
        settings = read_settings(x, x.pool.get_all()[0])
        if sends(settings):
            send_long(settings, "Weekly review of %s" % (snap.get("pool") or "the pool"), text, snap.get("pool") or "pool")
    try:
        save_json(state_path("review"), {"at": int(time.time()), "by": by, "text": text})
    except OSError:
        pass
    return text


# ---------------------------------------------------------------- the Advisor: good practice, checked

ADVICE_KEY = "svm-helper-advice"  # the checklist, for the app's Advisor page (JSON in the pool)
REQUEST_KEY = "svm-helper-request"  # what the app asks for: {"advice": when, "test": when}
TOPICS = ["backups", "storage", "servers", "vms", "security"]


def listed(items, most=3):
    """'a, b, c and 2 more'."""
    items = sorted(set(i for i in items if i))
    if len(items) <= most:
        return ", ".join(items)
    return "%s and %d more" % (", ".join(items[:most]), len(items) - most)


# Systems past their makers' support (as of late 2026), as the VM Tools report them: (pattern, name).
UNSUPPORTED_SYSTEMS = [
    (r"windows (server )?(2000|2003|2008)\b|windows (xp|vista)\b", "Windows"),
    (r"windows server 2012\b", "Windows Server 2012"),
    (r"windows (7|8|8\.1|10)\b", "Windows"),
    (r"centos( linux)?( release)? [5-8]\b", "CentOS"),
    (r"ubuntu (1[0-9]|20)\.(04|10)\b", "Ubuntu"),
    (r"debian( gnu/linux)? (8|9|10|11)\b", "Debian"),
    (r"red hat enterprise linux( server)?( release)? [5-7]\b", "Red Hat Enterprise Linux"),
]


def unsupported(os_name):
    """Whether a system's name (as its VM Tools give it) is one past its support."""
    n = (os_name or "").lower()
    return any(re.search(p, n) for p, _ in UNSUPPORTED_SYSTEMS)


def os_of(gm):
    """A VM's system, from its guest metrics: "Ubuntu 22.04.4 LTS", "Microsoft Windows Server 2019
    Standard"; "" when its VM Tools don't say."""
    v = (gm or {}).get("os_version") or {}
    name = (v.get("name") or "").split("|")[0].strip()
    if not name and v.get("distro"):
        name = ("%s %s.%s" % (v.get("distro"), v.get("major", ""), v.get("minor", ""))).strip(" .")
    return name


def advisor_checks(x, conf, state, now=None, load=None):
    """The pool against good practice, as far as the helper can tell by itself: a list of checks,
    each {id, topic, status (ok, warn or bad), title, note, objects}, in English (Claude then
    words them in the owner's language); one the app can fix also has fix: {action, uuids, names}
    (see the app's Advisor). `load`: week_load's. A part that can't be read is left out."""
    now = now or time.time()
    load = load or {"host": {}, "vm": {}}
    out = []

    def check(id_, topic, status, title, note="", objects=(), fix=None):
        c = {"id": id_, "topic": topic, "status": status, "title": title, "note": note if status != "ok" else "", "objects": sorted(set(o for o in objects if o))[:20]}
        if fix and status != "ok":
            c["fix"] = {"action": fix[0], "uuids": list(fix[1])[:50], "names": list(fix[2] if len(fix) > 2 else [])[:50]}
        out.append(c)

    def n_of(items, one, many):
        """`one` or `many`, by how many `items`, with their number where it says %d."""
        t = one if len(items) == 1 else many
        return t % len(items) if "%d" in t else t

    pool_ref = x.pool.get_all()[0]
    pool = x.pool.get_record(pool_ref)
    hosts = x.host.get_all_records()
    hm = x.host_metrics.get_all_records()
    srs = x.SR.get_all_records()
    vms = x.VM.get_all_records()
    settings = read_settings(x, pool_ref)

    def records(cls):
        try:
            return getattr(x, cls).get_all_records()
        except Exception:
            return {}

    vdis, vbds, gms, vmms = records("VDI"), records("VBD"), records("VM_guest_metrics"), records("VM_metrics")
    self_vm = conf.get("self_vm", "")
    real = {r: v for r, v in vms.items() if not (v.get("is_a_template") or v.get("is_a_snapshot") or v.get("is_control_domain"))}
    mine = {r: v for r, v in real.items() if v.get("uuid") != self_vm}  # (the helper's own VM aside)
    running = {r: v for r, v in mine.items() if v.get("power_state") == "Running"}
    live = [r for r, h in hosts.items() if hm.get(h.get("metrics"), {}).get("live")]

    def disks_of(ref):
        """A VM's disks: (VDI record, its SR's record)."""
        got = []
        for b in vms.get(ref, {}).get("VBDs") or []:
            vbd = vbds.get(b, {})
            d = vdis.get(vbd.get("VDI"))
            if vbd.get("type") == "Disk" and d:
                got.append((d, srs.get(d.get("SR"), {})))
        return got

    def part(fn):
        try:
            fn()
        except Exception as e:
            log("an Advisor check failed (%s): %s" % (fn.__name__, short_error(e)))

    def schedules():
        null = ("", "OpaqueRef:NULL")
        loose = [(v.get("uuid"), v.get("name_label")) for v in mine.values() if v.get("snapshot_schedule", "") in null]
        if loose:
            check("snapshot_schedules", "backups", "warn", n_of(loose, "%d VM has no snapshot schedule", "%d VMs have no snapshot schedule"), "A schedule takes snapshots by itself, so there's always a recent one to go back to after a bad update.", [n for _, n in loose], ("schedule", [u for u, _ in loose], [n for _, n in loose]))
        else:
            check("snapshot_schedules", "backups", "ok", "Every VM has a snapshot schedule")

    def storage():
        full, nearly = [], []
        for sr in srs.values():
            size, used = int(sr.get("physical_size", 0) or 0), int(sr.get("physical_utilisation", 0) or 0)
            if sr.get("content_type") == "iso" or sr.get("type") == "udev" or size <= 0:
                continue
            pct = 100.0 * used / size
            if pct >= 90:
                full.append("%s (%d%%)" % (sr.get("name_label"), pct))
            elif pct >= 80:
                nearly.append("%s (%d%%)" % (sr.get("name_label"), pct))
        if full:
            check("storage_space", "storage", "bad", "Storage nearly full: %s" % listed(full), "A full storage stops its VMs from writing, and new snapshots fail. Delete old snapshots or unused disks, or move a VM's disk elsewhere.", full + nearly)
        elif nearly:
            check("storage_space", "storage", "warn", "Storage filling up: %s" % listed(nearly), "Over 80% full: keep an eye on it, and delete old snapshots or unused disks.", nearly)
        else:
            check("storage_space", "storage", "ok", "Every storage has room to spare")
        unused, size = [], 0
        for d in vdis.values():
            sr = srs.get(d.get("SR"), {})
            if d.get("type") != "user" or d.get("is_a_snapshot") or d.get("VBDs") or not d.get("managed") or sr.get("content_type") == "iso" or d.get("is_tools_iso"):
                continue
            unused.append((d.get("uuid"), "%s (%s, on %s)" % (d.get("name_label") or "no name", gb(int(d.get("virtual_size", 0) or 0)), sr.get("name_label", "?"))))
            size += int(d.get("virtual_size", 0) or 0)
        if unused:
            check("unused_disks", "storage", "warn", "%s not used by any VM (%s)" % (n_of(unused, "%d disk", "%d disks"), gb(size)), "They take space and nothing uses them: delete the ones you don't need.", [n for _, n in unused], ("delete_disks", [u for u, _ in unused], [n for _, n in unused]))
        else:
            check("unused_disks", "storage", "ok", "No forgotten disks")
        old, old_snaps, many = [], [], []
        for v in mine.values():
            snaps = [vms.get(s, {}) for s in v.get("snapshots") or []]
            for s in snaps:
                age = (now - xen_epoch(s.get("snapshot_time"))) / 86400.0
                if not s.get("is_vmss_snapshot") and age > 30:
                    old.append(v.get("name_label"))
                    old_snaps.append((s.get("uuid"), "%s: %s (%d days old)" % (v.get("name_label"), s.get("name_label"), age)))
            if len(snaps) > 10:
                many.append(v.get("name_label"))
        if old_snaps:
            check("old_snapshots", "storage", "warn", n_of(old_snaps, "%d snapshot over a month old", "%d snapshots over a month old"), "A snapshot isn't a backup, and an old one keeps growing as the disk changes: delete the ones you no longer need.", old, ("delete_snapshots", [u for u, _ in old_snaps], [n for _, n in old_snaps]))
        else:
            check("old_snapshots", "storage", "ok", "No snapshots left over from long ago")
        if many:
            check("many_snapshots", "storage", "warn", n_of(many, "%d VM has more than 10 snapshots", "%d VMs have more than 10 snapshots"), "A long chain of snapshots slows the disk down. Keep fewer: a schedule's 'keep' does it by itself.", many)
        default = pool.get("default_SR", "")
        if default in ("", "OpaqueRef:NULL") or default not in srs:
            check("default_storage", "storage", "warn", "The pool has no default storage", "New VMs, and suspended ones, need a place to go: choose a default storage in VM Storage.", fix=("vm_storage", []))

    def servers():
        down = [h.get("name_label") for r, h in hosts.items() if r not in live]
        if down:
            check("servers_answering", "servers", "bad", "Not answering: %s" % listed(down), "The pool can't use it: its VMs aren't running. See whether it's on, and on the network.", down)
        else:
            check("servers_answering", "servers", "ok", n_of(hosts, "The server answers", "Every server answers"))
        drift = state.get("clock") or {}
        off = ["%s (%s)" % (h.get("name_label"), span(abs(drift[h.get("uuid")]))) for h in hosts.values() if abs(drift.get(h.get("uuid"), 0) or 0) > 60]
        if off:
            check("clocks", "servers", "bad", "Clock off: %s" % listed(off), "A wrong clock puts the wrong time on alerts and snapshots, and can break the pool's own certificates. Turn NTP on for that server.", off)
        elif drift:
            check("clocks", "servers", "ok", "Every server's clock is right")
        waiting = [h.get("name_label") for h in hosts.values() if h.get("latest_synced_updates_applied") == "no"]
        restart = [h.get("name_label") for h in hosts.values() if h.get("pending_guidances")]
        if waiting or restart:
            check("updates", "servers", "warn", "Updates waiting on %s" % listed(waiting + restart), "Updates fix security holes and bugs. Install them (a server may need a restart after).", waiting + restart, ("updates", []))
        elif any("latest_synced_updates_applied" in h for h in hosts.values()):
            check("updates", "servers", "ok", "Every server is up to date")
        versions = set("%s %s" % (h.get("software_version", {}).get("product_brand", ""), h.get("software_version", {}).get("product_version", "")) for h in hosts.values())
        if len(versions) > 1:
            check("same_version", "servers", "warn", "The servers run different versions", "A pool works best with every server on the same version: bring them all up to date.", [h.get("name_label") for h in hosts.values()], ("updates", []))
        disabled = [h.get("name_label") for h in hosts.values() if not h.get("enabled")]
        if disabled:
            check("maintenance", "servers", "warn", "In maintenance mode: %s" % listed(disabled), "No VM starts there until it's taken out of maintenance mode.", disabled)
        full = []
        for h in hosts.values():
            m = hm.get(h.get("metrics"), {})
            total, free = int(m.get("memory_total", 0) or 0), int(m.get("memory_free", 0) or 0)
            if total and (total - free) / total > 0.9:
                full.append(h.get("name_label"))
        if full:
            check("memory", "servers", "warn", "Memory nearly all used on %s" % listed(full), "No room left to start another VM there, or to move one over during maintenance.", full)
        else:
            check("memory", "servers", "ok", n_of(hosts, "The server has memory to spare", "Every server has memory to spare"))
        busy = ["%s (%d%% on average)" % (hosts[r].get("name_label"), load["host"][hosts[r].get("uuid")]["cpu"][0]) for r in live if (load["host"].get(hosts[r].get("uuid")) or {}).get("cpu", (0, 0))[0] >= 75]
        if busy:
            check("server_load", "servers", "warn", "Busy all week: %s" % listed(busy), "Its processors were busy most of the time: its VMs get slower. Move a VM to a quieter server.", busy)
        elif load["host"]:
            check("server_load", "servers", "ok", "The servers had processor to spare this week")
        # all the VMs on one server, the others idle: if it stops, everything stops
        if len(live) > 1 and len(running) > 1:
            where = set(v.get("resident_on") for v in running.values())
            if len(where) == 1:
                h = hosts.get(where.pop(), {})
                check("one_server", "servers", "warn", "Every running VM is on %s" % h.get("name_label", "one server"), "If that server stops, they all stop, while the others sit idle. Spread them over the servers.", [h.get("name_label")])

    def vm_care():
        none = [v.get("name_label") for v in running.values() if not gms.get(v.get("guest_metrics"), {}).get("PV_drivers_detected")]
        if none:
            check("vm_tools", "vms", "warn", n_of(none, "%d running VM without VM Tools", "%d running VMs without VM Tools"), "Without them a VM's disks and network are slower, it can't be shut down cleanly from outside, and its memory and IP address don't show. Install the XenServer VM Tools in it.", none)
        else:
            check("vm_tools", "vms", "ok", "Every running VM has its VM Tools")
        ordered = set(i.get("uuid") for i in ((settings.get("start_order") or {}).get("vms") or []) if (settings.get("start_order") or {}).get("on"))
        manual = [(v.get("uuid"), v.get("name_label")) for v in running.values() if v.get("uuid") not in ordered and (v.get("other_config") or {}).get("auto_poweron") != "true"]
        if manual:
            check("autostart", "vms", "warn", n_of(manual, "%d running VM doesn't start by itself", "%d running VMs don't start by themselves"), "After a power cut it stays off until someone starts it. Turn on 'Start with the server' for the VMs that should always run.", [n for _, n in manual], ("autostart", [u for u, _ in manual], [n for _, n in manual]))
        else:
            check("autostart", "vms", "ok", "The running VMs start by themselves after a power cut")
        # an ISO left in the drive: the VM can't move to a server that doesn't see that library
        isos = []
        for r, v in mine.items():
            for b in v.get("VBDs") or []:
                vbd = vbds.get(b, {})
                if vbd.get("type") == "CD" and not vbd.get("empty") and vbd.get("VDI") in vdis:
                    isos.append((v.get("uuid"), "%s (%s)" % (v.get("name_label"), vdis[vbd["VDI"]].get("name_label"))))
        if isos:
            check("iso_in_drive", "vms", "warn", n_of(isos, "%d VM has an ISO left in its drive", "%d VMs have an ISO left in their drive"), "It ties the VM to that library: it can't move to a server that doesn't see it, and the library can't be taken away. Eject the ones that aren't in use.", [n for _, n in isos], ("eject", [u for u, _ in isos], [n for _, n in isos]))
        else:
            check("iso_in_drive", "vms", "ok", "No ISO left in a VM's drive")
        # on a server's own storage: it can't move to another server while it runs
        if len(hosts) > 1:
            local = [v.get("name_label") for r, v in running.items() if any(not sr.get("shared") and sr.get("content_type") != "iso" for _, sr in disks_of(r))]
            if local:
                check("local_storage", "vms", "warn", n_of(local, "%d running VM can't move to another server", "%d running VMs can't move to another server"), "Their disks are on a server's own storage: for that server's updates or maintenance they have to be shut down. Moving their disks to shared storage lets them move while they run.", local)
            elif running:
                check("local_storage", "vms", "ok", "Every running VM can move to another server")
        old_os = ["%s (%s)" % (v.get("name_label"), os_of(gms.get(v.get("guest_metrics")))) for v in mine.values() if unsupported(os_of(gms.get(v.get("guest_metrics"))))]
        if old_os:
            check("os_support", "vms", "warn", n_of(old_os, "%d VM runs a system past its support", "%d VMs run a system past their support"), "It gets no more security updates from its makers. Plan to move what it does to a supported version.", old_os)
        elif any(os_of(gms.get(v.get("guest_metrics"))) for v in running.values()):
            check("os_support", "vms", "ok", "The VMs' systems are still supported")
        # off for months, still taking disk
        idle, size = [], 0
        for r, v in mine.items():
            if v.get("power_state") != "Halted":
                continue
            started = xen_epoch(vmms.get(v.get("metrics"), {}).get("start_time"))
            if started and now - started > 90 * 86400:
                idle.append("%s (started %d days ago)" % (v.get("name_label"), (now - started) / 86400))
                size += sum(int(d.get("physical_utilisation", 0) or 0) for d, _ in disks_of(r))
        if idle:
            check("vms_off_long", "vms", "warn", "%s for months (%s of disk)" % (n_of(idle, "%d VM off", "%d VMs off"), gb(size)), "Their disks still take room. Export them to a file and delete them if they're not needed, or keep them if they're spares.", idle)
        # the week's load
        hot, big = [], []
        for v in running.values():
            w = load["vm"].get(v.get("uuid")) or {}
            cpu, mem = w.get("cpu"), w.get("memory")
            if (cpu and cpu[0] >= 80) or (mem and mem[0] >= 90):
                hot.append("%s (%s)" % (v.get("name_label"), ", ".join(t for t in [("processors %d%%" % cpu[0]) if cpu and cpu[0] >= 80 else "", ("memory %d%%" % mem[0]) if mem and mem[0] >= 90 else ""] if t)))
            vcpus = int(v.get("VCPUs_max", 0) or 0)
            if cpu and vcpus >= 4 and cpu[1] < 15:
                big.append("%s (%d vCPUs, at most %d%% busy)" % (v.get("name_label"), vcpus, cpu[1]))
        if hot:
            check("vm_busy", "vms", "warn", n_of(hot, "%d VM at its limit all week", "%d VMs at their limit all week"), "On average over the week, it used nearly all its processors or memory: it's slower than it should be. Give it more (it has to be shut down for that).", hot)
        elif load["vm"]:
            check("vm_busy", "vms", "ok", "No VM at its limit this week")
        if big:
            check("vm_idle_cpus", "vms", "warn", n_of(big, "%d VM has more vCPUs than it uses", "%d VMs have more vCPUs than they use"), "Its processors stayed nearly idle all week. Fewer vCPUs leave more for the other VMs, and it won't be slower.", big)

    def security():
        try:
            certs = x.Certificate.get_all_records()
        except Exception:
            certs = {}
        soon, expiring = [], []
        for c in certs.values():
            if not xen_epoch(c.get("not_after")):
                continue
            left = (xen_epoch(c.get("not_after")) - now) / 86400.0
            h = hosts.get(c.get("host"), {})
            who = (h.get("uuid"), h.get("name_label") or c.get("name") or "?")
            if left < 30:
                soon.append(who)
            elif left < 90:
                expiring.append(who)
        if soon:
            check("certificates", "security", "bad", "Certificates expire within a month: %s" % listed([n for _, n in soon]), "After that, apps can't connect to the server safely. Renew it, then run 'sudo svm-helper trust' on the Helper VM.", [n for _, n in soon], ("renew_cert", [u for u, _ in soon if u], [n for _, n in soon]))
        elif expiring:
            check("certificates", "security", "warn", "Certificates expire within three months: %s" % listed([n for _, n in expiring]), "Plan to renew them, then run 'sudo svm-helper trust' on the Helper VM.", [n for _, n in expiring], ("renew_cert", [u for u, _ in expiring if u], [n for _, n in expiring]))
        elif certs:
            check("certificates", "security", "ok", "The servers' certificates are valid for months")
        if sends(settings):
            check("alerts_sent", "security", "ok", "Alerts reach your phone")
        else:
            check("alerts_sent", "security", "warn", "Alerts don't reach your phone", "A failing disk or a server down would go unnoticed. Choose where alerts go in the pool's Helper tab.", fix=("helper_tab", []))

    for fn in (schedules, storage, servers, vm_care, security):
        part(fn)
    return out


def json_answer(text):
    """The JSON object in a model's answer (with or without a code fence around it); None."""
    if not text:
        return None
    a, b = text.find("{"), text.rfind("}")
    if a < 0 or b <= a:
        return None
    try:
        v = json.loads(text[a:b + 1])
    except ValueError:
        return None
    return v if isinstance(v, dict) else None


def worded(checks, words):
    """The checks with Claude's words: each one's title and note in the owner's language, and
    up to ten findings of its own (by: claude)."""
    out = [dict(c) for c in checks]
    w = words.get("checks") if isinstance(words.get("checks"), dict) else {}
    for c in out:
        said = w.get(c["id"])
        if not isinstance(said, dict):
            continue
        if isinstance(said.get("title"), str) and said["title"].strip():
            c["title"] = said["title"].strip()[:140]
        if isinstance(said.get("note"), str) and (said["note"].strip() or c["status"] == "ok"):
            c["note"] = said["note"].strip()[:800]
    more = words.get("more") if isinstance(words.get("more"), list) else []
    n = 0
    for m in more:
        if n >= 10 or not isinstance(m, dict) or m.get("topic") not in TOPICS or m.get("status") not in ("ok", "warn", "bad"):
            continue
        if not isinstance(m.get("title"), str) or not m["title"].strip():
            continue
        n += 1
        out.append({"id": "claude_%d" % n, "topic": m["topic"], "status": m["status"], "title": m["title"].strip()[:140], "note": str(m.get("note") or "").strip()[:800], "objects": [], "by": "claude"})
    return out


def write_advice(pool, conf, secrets, request=0):
    """The Advisor's checklist: the checks, worded by Claude when it can be; left in the pool
    (ADVICE_KEY) for the app, and its outline in advice.json. The checklist."""
    x = pool.x
    state = load_json(state_path("state"), {})
    load = week_load(pool)
    checks = advisor_checks(x, conf, state, load=load)
    claude = Claude(conf, secrets)
    by = "plain"
    if claude.ready():
        words = None
        try:
            words = json_answer(claude.advise(checks, snapshot_of_pool(x, state, load)))
        except Exception as e:
            log("Claude couldn't look at the Advisor's checklist: %s" % short_error(e))
        if words:
            checks, by = worded(checks, words), "claude"
        else:
            log("Claude's words for the Advisor didn't come: its checklist is plain this time")
    advice = {"version": 1, "at": int(time.time()), "by": by, "language": conf.get("language", "en") if by == "claude" else "en", "request": request, "checks": checks}
    write_other_config(x, "pool", x.pool.get_all()[0], ADVICE_KEY, json.dumps(advice, ensure_ascii=False, sort_keys=True))
    try:
        save_json(state_path("advice"), {"at": advice["at"], "by": by, "request": request, "bad": sum(c["status"] == "bad" for c in checks), "warn": sum(c["status"] == "warn" for c in checks)})
    except OSError:
        pass
    log("the Advisor's checklist: %d to fix, %d to look at, %d fine" % (sum(c["status"] == "bad" for c in checks), sum(c["status"] == "warn" for c in checks), sum(c["status"] == "ok" for c in checks)))
    return advice


def requests_of(pool_rec):
    """What the app asked the helper for, in the pool: {"advice"|"test"|"update": when}."""
    v = request_record(pool_rec)
    return {k: float(t) for k, t in v.items() if isinstance(t, (int, float)) and not isinstance(t, bool)}


def request_record(pool_rec):
    try:
        v = json.loads((pool_rec.get("other_config") or {}).get(REQUEST_KEY, "") or "{}")
    except ValueError:
        return {}
    return v if isinstance(v, dict) else {}


UPDATE_REF = re.compile(r"main|v\d+\.\d+\.\d+")
UPDATER = os.path.join(APP_DIR, "update.sh")  # run as root by svm-helper-update.service


def can_update():
    """Whether this install updates itself when the app asks (setup.sh from 2.3 puts it there)."""
    return os.access(UPDATER, os.X_OK)


def ask_update(ref):
    """Leaves the update the app asked for, for svm-helper-update.path to start (as root: the
    helper can't install anything itself). Whether it could."""
    if not UPDATE_REF.fullmatch(ref or "") or not can_update():
        return False
    with open(state_path("update").replace(".json", ".request"), "w") as f:
        f.write(ref + "\n")
    return True


def send_test(pool, settings, pool_name, asked):
    """The app's "Send a test": a test alert to the channels as saved; what each said, in
    test.json and in the heartbeat, for the app."""
    res = send(settings, "A test from the Helper VM", "If you see this, alerts reach you here.", 3, pool_name) if sends(settings) else {}
    result = {"request": asked, "at": int(time.time()), "results": res}
    try:
        save_json(state_path("test"), result)
    except OSError:
        pass
    x = pool.x
    ref = x.pool.get_all()[0]
    try:
        beat = json.loads(x.pool.get_other_config(ref).get(STATUS_KEY, "") or "{}")
    except ValueError:
        beat = {}
    beat["test"] = result
    write_other_config(x, "pool", ref, STATUS_KEY, json.dumps(beat, sort_keys=True))
    log("a test alert, as the app asked: %s" % (", ".join("%s %s" % (k, v) for k, v in res.items()) or "no channel is on"))


# ---------------------------------------------------------------- the service

class Helper:
    """The service: a look each minute, and the jobs that take longer in their own threads (each
    with its own session)."""

    def __init__(self, jobs=True):
        self.started = time.time()
        self.stop = threading.Event()
        self.pool = None
        self.fails = 0
        self.jobs = {}
        self.run_jobs = jobs
        self.want_reload = False
        self.reload()

    def reload(self):
        self.conf = load_config()
        self.secrets = load_secrets()
        if self.pool:
            self.pool.close()
            self.pool = None

    def spawn(self, name, fn):
        """`fn(pool)` in its own thread with its own session, unless one by that name still runs."""
        t = self.jobs.get(name)
        if not self.run_jobs or (t and t.is_alive()):
            return False

        def run():
            p = None
            try:
                p = Pool(self.conf, self.secrets).connect()
                fn(p)
            except Exception as e:
                log("%s failed: %s" % (name, short_error(e)))
                log(traceback.format_exc().strip().replace("\n", " | "))
            finally:
                if p:
                    p.close()

        t = threading.Thread(target=run, name=name, daemon=True)
        self.jobs[name] = t
        t.start()
        return True

    def unreachable(self, state, why, now):
        """The pool can't be reached: after three minutes, straight to the phone (with the
        settings last read), once, and again when it's back."""
        self.fails += 1
        if self.fails == 1 or self.fails % 30 == 0:
            log("can't reach the pool: %s" % why)
        if self.fails == 3:
            seen = state.get("hosts_seen", {})
            answering = [n for a, n in seen.items() if tcp_ok(a, 443, 3)]
            silent = [n for a, n in seen.items() if n not in answering]
            text = why
            if seen:
                text += "\nAnswering: %s\nNot answering: %s" % (", ".join(answering) or "none", ", ".join(silent) or "none")
            state["unreachable_since"] = now - 180
            add_event(state, now, "Couldn't reach the pool: " + why)
            settings = state.get("settings", {})
            if sends(settings):
                send(settings, "Can't reach the pool %s" % state.get("pool_name", ""), text, 1, state.get("pool_name", "pool"))

    def look(self, state, now):
        if self.pool is None:
            self.pool = Pool(self.conf, self.secrets).connect()
        look = Look(self.pool, self.conf, self.secrets, state, now)
        lost = state.pop("unreachable_since", None)
        if lost is not None:
            look.alert("SVM_POOL_BACK", 3, "Pool", look.pool_uuid, "Reached the pool again, after %s" % span(now - lost))
        self.fails = 0
        done = []
        steps = [remount, storage, schedules, hosts_watch, services, clocks, forward]
        for step in steps:
            try:
                done += step(look) or []
            except Exception as e:
                log("%s failed: %s" % (step.__name__, short_error(e)))
                log(traceback.format_exc().strip().replace("\n", " | "))
        # once after the pool starts: the VMs in order
        so = look.settings.get("start_order") or {}
        try:
            boot = float((look.hosts.get(look.master, {}).get("other_config") or {}).get("boot_time", "0"))
        except ValueError:
            boot = 0
        if self.run_jobs and so.get("on") and so.get("vms") and boot and state.get("order_boot") != boot:
            state["order_boot"] = boot
            if now - boot < 30 * 60:
                settings = dict(look.settings)
                self.spawn("start order", lambda p: start_in_order(p, settings, boot))
                done.append("starting the VMs in order")
        rv = (self.conf.get("claude") or {}).get("review") or {}
        rs = load_json(state_path("review"), {})
        if self.run_jobs and rv.get("on", True) and due(rv.get("at", "08:00"), [rv.get("day", "Monday")], max(rs.get("at", 0), state.get("review_kicked", 0)), now):
            state["review_kicked"] = now
            conf, sec = self.conf, self.secrets
            if self.spawn("weekly review", lambda p: write_review(p, conf, sec)):
                done.append("writing the weekly review")
        # the Advisor: when the app asks (its "Check now"), and once a day
        asked = requests_of(look.pool_rec)
        answered = state.setdefault("answered", {})
        ad = (self.conf.get("claude") or {}).get("advice") or {}
        ads = load_json(state_path("advice"), {})
        conf, sec = self.conf, self.secrets
        if self.run_jobs and asked.get("advice", 0) > answered.get("advice", 0):
            t = asked["advice"]
            if self.spawn("advisor", lambda p, t=t: write_advice(p, conf, sec, t)):
                answered["advice"] = t
                done.append("checking the pool for the Advisor, as the app asked")
        elif self.run_jobs and ad.get("on", True) and due(ad.get("at", "07:00"), [], max(ads.get("at", 0), state.get("advice_kicked", 0)), now):
            state["advice_kicked"] = now
            if self.spawn("advisor", lambda p: write_advice(p, conf, sec)):
                done.append("checking the pool for the Advisor")
        if self.run_jobs and asked.get("test", 0) > answered.get("test", 0):
            t, settings, name = asked["test"], dict(look.settings), look.pool_name
            if self.spawn("test alert", lambda p, t=t: send_test(p, settings, name, t)):
                answered["test"] = t
        if self.run_jobs and asked.get("update", 0) > answered.get("update", 0):
            answered["update"] = asked["update"]
            ref = str(request_record(look.pool_rec).get("update_ref") or "")
            if ask_update(ref):
                done.append("updating to %s, as the app asked" % ref)
            else:
                log("the app asked for an update to '%s', which can't be done here" % ref)
        state["last_look"] = now
        heartbeat(look, self.started)
        look.save()
        return done

    def tick(self):
        if self.want_reload:
            self.want_reload = False
            self.reload()
            log("settings read again")
        now = time.time()
        with TickLock():
            state = load_json(state_path("state"), {})
            state["version"] = VERSION
            try:
                done = self.look(state, now)
                for line in done:
                    log(line)
                return done or ["all well"]
            except CertificateChanged as e:
                if now - state.get("cert_alert", 0) > 86400:
                    state["cert_alert"] = now
                    if sends(state.get("settings", {})):
                        send(state["settings"], "The pool's certificate changed", str(e), 2, state.get("pool_name", "pool"))
                log(str(e))
                self.drop()
                return [str(e)]
            except Exception as e:
                self.unreachable(state, short_error(e), now)
                self.drop()
                return ["can't reach the pool: %s" % short_error(e)]
            finally:
                try:
                    save_json(state_path("state"), state)
                except OSError:
                    pass

    def drop(self):
        if self.pool:
            self.pool.close()
        self.pool = None

    def run(self):
        global AS_SERVICE
        AS_SERVICE = True
        signal.signal(signal.SIGTERM, lambda *a: self.stop.set())
        signal.signal(signal.SIGHUP, lambda *a: setattr(self, "want_reload", True))
        log("Helper VM %s started" % VERSION)
        if not (self.conf.get("pool") or {}).get("hosts"):
            log("no pool set yet: run 'sudo svm-helper setup'")
        if self.run_jobs:
            threading.Thread(target=copies, args=(self,), name="copies", daemon=True).start()
        while not self.stop.is_set():
            t0 = time.time()
            try:
                self.tick()
            except Exception as e:
                log("its look failed: %s" % short_error(e))
            self.stop.wait(max(5, 60 - (time.time() - t0)))
        self.drop()
        log("Helper VM stopped")


# ---------------------------------------------------------------- disk copies (VM Conversion)

COPY_JOB = "svm-copy"
COPY_CLAIMED = "svm-copy-claimed"
COPY_ERROR = "svm-copy-error"


def askpass_script():
    """A tiny program that answers ssh's password question from the environment."""
    path = os.path.join(STATE_DIR, "askpass.sh")
    if not os.path.exists(path):
        with open(path, "w") as f:
            f.write('#!/bin/sh\nprintf \'%s\\n\' "$SVM_SSH_PASSWORD"\n')
        os.chmod(path, 0o700)
    return path


def open_source(job, password):
    """A disk's bytes at its source, as Simple VM Manager's job says: (something with .read(n),
    its size, what ends it). "http": an ESXi datastore's flat file; "ssh": a command on a Proxmox
    server that prints the size on its first line, then the bytes."""
    kind = job.get("kind")
    size = int(job.get("size") or 0)
    if kind == "http":
        req = urllib.request.Request(job["url"], headers={"User-Agent": "svm-helper/" + VERSION})
        token = base64.b64encode(("%s:%s" % (job.get("user", ""), password)).encode()).decode()
        req.add_header("Authorization", "Basic " + token)
        try:
            r = urllib.request.urlopen(req, context=unverified_context(), timeout=120)
        except urllib.error.HTTPError as e:
            raise PoolError("the source said %d %s" % (e.code, e.reason))
        n = int(r.headers.get("Content-Length") or 0)
        if size and n and n != size:
            r.close()
            raise PoolError("the source sends %d bytes for a disk of %d" % (n, size))
        return r, n or size, r.close
    if kind == "ssh":
        home = os.environ.get("HOME") or STATE_DIR
        known = os.path.join(home, ".ssh", "known_hosts")
        os.makedirs(os.path.dirname(known), mode=0o700, exist_ok=True)
        env = dict(os.environ, SSH_ASKPASS=askpass_script(), SSH_ASKPASS_REQUIRE="force", DISPLAY=os.environ.get("DISPLAY", ":0"), SVM_SSH_PASSWORD=password)
        cmd = [
            "ssh", "-T", "-o", "BatchMode=no", "-o", "StrictHostKeyChecking=accept-new", "-o", "UserKnownHostsFile=" + known,
            "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=30", "-o", "NumberOfPasswordPrompts=1",
            "-o", "PubkeyAuthentication=no", "-o", "PreferredAuthentications=password,keyboard-interactive",
            "-p", str(job.get("port") or 22), "-l", job.get("user") or "root", job["host"], job["command"],
        ]
        try:
            proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, start_new_session=True)
        except FileNotFoundError:
            raise PoolError("this machine has no ssh client: run setup.sh again (it installs one)")

        def end():
            proc.kill()
            proc.wait()

        first = proc.stdout.readline()
        try:
            n = int(first.strip())
        except ValueError:
            end()
            said = [l for l in proc.stderr.read().decode(errors="replace").splitlines() if l.strip() and "Permanently added" not in l]
            raise PoolError("ssh to %s: %s" % (job["host"], (said[-1] if said else first.decode(errors="replace").strip()) or "no answer"))
        if size and n != size:
            end()
            raise PoolError("the disk is %d bytes there, not %d" % (n, size))
        return proc.stdout, n, end
    raise PoolError("a copy of a kind not known (%s)" % kind)


def copy_disk(pool, task, job, password):
    """One disk into its VDI: the source's bytes sent on to PUT /import_raw_vdi with the app's
    task, which the pool moves along and ends (the app follows it, and destroys it)."""
    vdi = pool.x.VDI.get_by_uuid(job["vdi"])
    src, size, end = open_source(job, password)
    try:
        pool.upload("/import_raw_vdi", {"vdi": vdi, "task_id": task, "format": job.get("format") or "raw"}, src, size, address=job.get("address") or None)
    finally:
        end()
    return size


def copy_job(helper, task, job):
    """A claimed copy, in its own thread with its own session; why it failed goes on the task."""
    p = None
    try:
        p = Pool(helper.conf, helper.secrets).connect()
        password = p.x.secret.get_value(p.x.secret.get_by_uuid(job["secret"])) if job.get("secret") else ""
        log("copying a disk for Simple VM Manager (%s, %s)" % (job.get("kind"), gb(int(job.get("size") or 0))))
        t0 = time.time()
        size = copy_disk(p, task, job, password)
        log("copied a disk for Simple VM Manager (%s in %s)" % (gb(size), span(time.time() - t0)))
    except Exception as e:
        why = short_error(e)
        log("a disk copy failed: %s" % why)
        try:
            p = p or Pool(helper.conf, helper.secrets).connect()
            p.x.task.add_to_other_config(task, COPY_ERROR, why)
        except Exception:
            pass
    finally:
        if p:
            p.close()


def claim_copies(x, machine, running):
    """The copies Simple VM Manager asked for and nobody took yet: each claimed (a key XAPI adds
    once only, so two helpers can't both take one). (task, job) of those claimed."""
    out = []
    for ref, t in x.task.get_all_records().items():
        oc = t.get("other_config") or {}
        if COPY_JOB not in oc or COPY_CLAIMED in oc or t.get("status") != "pending" or ref in running:
            continue
        try:
            job = json.loads(oc[COPY_JOB])
        except ValueError:
            continue
        try:
            x.task.add_to_other_config(ref, COPY_CLAIMED, machine)
        except Exception:
            continue  # (another took it first)
        out.append((ref, job))
    return out


def copies(helper):
    """The copies thread: every few seconds, the disk copies Simple VM Manager hands over (tasks
    carrying "svm-copy"), each run in its own thread."""
    machine = socket.gethostname()
    running = {}
    pool = None
    while not helper.stop.is_set():
        if not (helper.conf.get("pool") or {}).get("hosts"):
            helper.stop.wait(30)
            continue
        try:
            if pool is None:
                pool = Pool(helper.conf, helper.secrets).connect()
            for ref, job in claim_copies(pool.x, machine, running):
                t = threading.Thread(target=copy_job, args=(helper, ref, job), name="copy", daemon=True)
                running[ref] = t
                t.start()
            for ref in [r for r, t in running.items() if not t.is_alive()]:
                del running[ref]
            helper.stop.wait(3)
        except CertificateChanged as e:
            log("disk copies stopped: %s" % e)
            return
        except Exception as e:
            log("disk copies: %s" % short_error(e))
            if pool:
                pool.close()
            pool = None
            helper.stop.wait(30)
    if pool:
        pool.close()


class TickLock:
    """One look at a time (the service's, or 'svm-helper check')."""

    def __enter__(self):
        self.f = open(os.path.join(STATE_DIR, "look.lock"), "w")
        fcntl.flock(self.f, fcntl.LOCK_EX)
        return self

    def __exit__(self, *a):
        fcntl.flock(self.f, fcntl.LOCK_UN)
        self.f.close()


# ---------------------------------------------------------------- the command line

def need_pool():
    conf, sec = load_config(), load_secrets()
    if not (conf.get("pool") or {}).get("hosts"):
        die("No pool set yet: run 'sudo svm-helper setup' first.")
    try:
        return ui.spin("Connecting to the pool", lambda: Pool(conf, sec).connect()), conf, sec
    except (PoolError, CertificateChanged, OSError):
        sys.exit(1)  # (the spinner said why)


def die(text):
    print("  " + ui.red(ui.cross) + " " + text)
    sys.exit(1)


def cmd_status(args):
    conf = load_config()
    state = load_json(state_path("state"), {})
    r = load_json(state_path("review"), {})
    active = subprocess.run(["systemctl", "is-active", SERVICE], capture_output=True, text=True).stdout.strip() or "unknown"
    ui.banner("Helper VM %s" % VERSION, "looking after the pool '%s'" % state.get("pool_name", "?"))
    print()
    dot = ui.green("●") if active == "active" else ui.red("●")
    print("  %s Service: %s" % (dot, active))
    last = state.get("last_look")
    print("  %s Last look: %s" % (ui.green("●") if last and time.time() - last < 180 else ui.yellow("●"), ("%s ago" % span(time.time() - last)) if last else "never"))
    ch = [k for k, v in (state.get("settings", {}).get("channels") or {}).items() if (v or {}).get("on")]
    print("  %s Alerts go to: %s" % (ui.green("●") if ch else ui.yellow("●"), ", ".join(ch) or "nowhere yet (sudo svm-helper setup)"))
    cl = Claude(conf, load_secrets()) if os.access(SECRETS, os.R_OK) or not os.path.exists(SECRETS) else None
    print("  %s Claude: %s" % (ui.green("●") if cl and cl.ready() else ui.dim("●"), ("on, through " + cl.how()) if cl and cl.ready() else ("off" if cl and cl.mode == "off" else "not working (sudo svm-helper setup)" if cl else "?")))
    rv = (conf.get("claude") or {}).get("review") or {}
    print("  %s Weekly review: %s" % (ui.green("●") if rv.get("on", True) else ui.dim("●"), ("%ss at %s; last %s" % (rv.get("day", "Monday"), rv.get("at", "08:00"), ("%s ago" % span(time.time() - r["at"])) if r.get("at") else "never")) if rv.get("on", True) else "off"))
    ad = load_json(state_path("advice"), {})
    print("  %s Advisor: %s" % (ui.red("●") if ad.get("bad") else ui.yellow("●") if ad.get("warn") else ui.green("●") if ad.get("at") else ui.dim("●"), ("last checked %s ago: %d to fix, %d to look at" % (span(time.time() - ad["at"]), ad.get("bad", 0), ad.get("warn", 0))) if ad.get("at") else "not checked yet ('svm-helper advise')"))
    svc = (conf.get("watch") or {}).get("services") or []
    if svc:
        down = [s.get("name") for s in svc if (state.get("services", {}).get(s.get("url")) or {}).get("down_since")]
        print("  %s Watching: %s%s" % (ui.red("●") if down else ui.green("●"), ", ".join(s.get("name", s.get("url")) for s in svc), ("  (down: %s)" % ", ".join(down)) if down else ""))
    print()
    print("  " + ui.bold("Lately"))
    for line in log_tail(int(args[0]) if args and args[0].isdigit() else 15):
        print("    " + ui.dim(line[:19]) + line[19:])
    print()


def cmd_check(args):
    if not (load_config().get("pool") or {}).get("hosts"):
        die("No pool set yet: run 'sudo svm-helper setup' first.")
    h = Helper(jobs=False)
    done = ui.spin("One look at the pool", h.tick)
    h.drop()
    for line in done:
        ui.info(line)


def cmd_test(args):
    pool, conf, sec = need_pool()
    x = pool.x
    p = x.pool.get_all()[0]
    settings = read_settings(x, p)
    if not sends(settings):
        die("No channel is on: run 'sudo svm-helper setup' to choose where alerts go.")
    res = ui.spin("Sending a test alert", lambda: send(settings, "A test from the Helper VM", "If you see this, alerts reach you here.", 3, x.pool.get_name_label(p) or "pool"))
    for k, v in res.items():
        (ui.ok if v == "ok" else ui.fail)("%s: %s" % (k, "reached" if v == "ok" else v))
    pool.close()


def cmd_review(args):
    pool, conf, sec = need_pool()
    send_it = "--send" in args
    text = ui.spin("Writing the review" + (" with Claude" if Claude(conf, sec).ready() else ""), lambda: write_review(pool, conf, sec, send_it))
    print()
    for line in text.splitlines():
        print("    " + line)
    print()
    if not send_it:
        ui.note("Not sent: 'svm-helper review --send' sends it to your channels too.")
    pool.close()


def cmd_advise(args):
    pool, conf, sec = need_pool()
    advice = ui.spin("Checking the pool" + (" with Claude" if Claude(conf, sec).ready() else ""), lambda: write_advice(pool, conf, sec))
    pool.close()
    marks = {"ok": ui.green(ui.tick), "warn": ui.yellow("!"), "bad": ui.red(ui.cross)}
    for topic in TOPICS:
        checks = [c for c in advice["checks"] if c["topic"] == topic]
        if not checks:
            continue
        print("\n  " + ui.bold(topic.capitalize()))
        for c in checks:
            print("    %s %s" % (marks.get(c["status"], "?"), c["title"]))
            for line in textwrap.wrap(c.get("note") or "", 70):
                print("        " + ui.dim(line))
    print()
    ui.note("Left in the pool too: Simple VM Manager's Advisor page shows it.")


def cmd_xapi(args):
    """One Python expression on the pool's API (x is session.xenapi): its result as JSON."""
    if not args:
        die("Usage: svm-helper xapi 'x.VM.get_all_records()'")
    pool, conf, sec = need_pool()
    env = {"x": pool.x, "pool": pool, "json": json, "time": time}
    try:
        v = eval(" ".join(args), env)
    except Exception as e:
        pool.close()
        die("%s: %s" % (e.__class__.__name__, short_error(e)))
    print(json.dumps(v, indent=1, default=str, ensure_ascii=False))
    pool.close()


def cmd_shell(args):
    pool, conf, sec = need_pool()
    banner = "The pool's API: x is session.xenapi (try x.VM.get_all_records()); Ctrl-D leaves."
    try:
        code.interact(banner=banner, local={"x": pool.x, "pool": pool, "json": json})
    finally:
        pool.close()


def cmd_trust(args):
    """The servers' certificates read again and pinned (after one was renewed)."""
    conf = load_config()
    hosts = (conf.get("pool") or {}).get("hosts") or []
    if not hosts:
        die("No pool set yet: run 'sudo svm-helper setup' first.")
    pins = []
    for a in hosts:
        try:
            fp = cert_fingerprint(a)
        except OSError as e:
            ui.warn("%s: %s" % (a, short_error(e)))
            continue
        l1, l2 = pretty_fingerprint(fp)
        print("        %s  %s\n        %s  %s" % (ui.bold(a.ljust(16)), l1, " " * 16, l2))
        pins.append(fp)
    if pins and ui.yes("Trust these certificates?", True):
        conf["pool"]["pins"] = sorted(set(pins))
        save_json(CONFIG, conf, 0o640)
        chown_conf(CONFIG)
        restart_service()
        ui.ok("Saved; the service uses them from now on.")


def restart_service():
    subprocess.run(["systemctl", "restart", SERVICE], capture_output=True)


# ---------------------------------------------------------------- setup: the questions

def chown_conf(path):
    try:
        os.chown(path, 0, grp.getgrnam(USER).gr_gid)
    except (KeyError, OSError):
        pass


def asked(conf, key, prompt, default=True):
    """A yes or no that's asked again at each setup: Enter keeps the last answer (`default` the
    first time)."""
    answers = conf.setdefault("answers", {})
    said = ui.yes(prompt, answers.get(key, default))
    answers[key] = said
    return said


def save_conf(conf, secrets_):
    os.makedirs(CONF_DIR, exist_ok=True)
    save_json(CONFIG, conf, 0o640)
    save_json(SECRETS, secrets_, 0o640)
    chown_conf(CONFIG)
    chown_conf(SECRETS)


def ask_address(v):
    if not v:
        return "type the server's name or IP address"
    if not re.fullmatch(r"[A-Za-z0-9.:_-]+", v):
        return "that doesn't look like a name or IP address"
    return None


def setup_pool(n, total, conf, sec):
    ui.step(n, total, "Your pool")
    ui.note("Any of its servers will do: a member sends this helper on to the coordinator. It logs in like Simple VM Manager does, with the pool's root user.")
    p = conf["pool"]
    address = ui.ask("Server address", (p.get("hosts") or [""])[0], check=ask_address)
    user = ui.ask("User", p.get("user") or "root")
    had = bool(sec.get("pool_password")) and address in (p.get("hosts") or [])
    password = ui.ask("Password", sec.get("pool_password", "") if had else "", secret=True, keep_hint="Enter keeps the saved one" if had else "", check=lambda v: None if v else "type the password")

    pins = set(p.get("pins") or [])

    def meet(a):
        """A server's certificate, trusted when already pinned, or when the person says so."""
        fp = ui.spin("Reading %s's certificate" % a, lambda: cert_fingerprint(a))
        if fp in pins:
            return fp
        l1, l2 = pretty_fingerprint(fp)
        print("          SHA-256  " + ui.bold(l1) + "\n                   " + ui.bold(l2))
        ui.note("This is how this machine will know it's really talking to your servers. If you want to be sure, compare it with the server's own: in its console, 'openssl x509 -in /etc/xensource/xapi-ssl.pem -noout -fingerprint -sha256'.")
        if not ui.yes("Is this your server?", True):
            raise KeyboardInterrupt
        pins.add(fp)
        return fp

    import XenAPI
    target = address
    while True:
        try:
            meet(target)
        except OSError:
            ui.note("Check the address, and that this machine reaches the server (it uses HTTPS, port 443).")
            return setup_pool(n, total, conf, sec)
        s = XenAPI.Session("https://" + bracket(target), transport=PinnedTransport(pins))
        try:
            s.xenapi.login_with_password(user, password, "1.0", "svm-helper-setup")
            ui.ok("Logged in to %s as %s" % (target, user))
            break
        except XenAPI.Failure as f:
            d = list(f.details or [])
            if d and d[0] == "HOST_IS_SLAVE" and len(d) > 1:
                ui.info("%s is a member: its coordinator is %s" % (target, d[1]))
                target = d[1]
                continue
            if d and d[0] == "SESSION_AUTHENTICATION_FAILED":
                ui.fail("The pool refused that user or password.")
                return setup_pool(n, total, conf, sec)
            raise
    x = s.xenapi
    pool_ref = x.pool.get_all()[0]
    pool_name = x.pool.get_name_label(pool_ref) or "(no name)"
    hosts = x.host.get_all_records()
    vms = {r: v for r, v in x.VM.get_all_records().items() if not v["is_a_template"] and not v["is_a_snapshot"] and not v["is_control_domain"]}
    brand = next(iter(hosts.values()), {}).get("software_version", {})
    ui.ok("Pool %s: %d server%s, %d VMs, %s %s" % (ui.bold(pool_name), len(hosts), "" if len(hosts) == 1 else "s", len(vms), brand.get("product_brand", ""), brand.get("product_version", "")))
    addresses = [target]
    master = x.pool.get_master(pool_ref)
    for ref, h in sorted(hosts.items(), key=lambda kv: kv[0] != master):
        a = h.get("address", "")
        if a and a not in addresses:
            addresses.append(a)
        try:
            pins.add(cert_fingerprint(a))
            ui.ok("%s %s%s" % (h.get("name_label"), ui.dim(a), ui.dim("  · coordinator") if ref == master else ""))
        except OSError as e:
            ui.warn("%s %s: not answering now (%s)" % (h.get("name_label"), a, short_error(e)))
    conf["pool"] = {"hosts": addresses, "user": user, "pins": sorted(pins)}
    sec["pool_password"] = password

    # this machine, as one of the pool's VMs
    me = this_vm_uuid()
    mine = None
    if me:
        for ref, v in vms.items():
            if v.get("uuid", "").lower() == me:
                mine = (ref, v)
    if mine:
        ref, v = mine
        conf["self_vm"] = v["uuid"]
        where = hosts.get(v.get("resident_on"), {}).get("name_label", "?")
        ui.ok("This machine is the VM %s, on %s" % (ui.bold(v["name_label"]), where))
        if (v.get("other_config") or {}).get("auto_poweron") != "true":
            ui.note("After a power cut, the helper should be up early: it's the one that starts the other VMs in order and tells you what's wrong.")
            if asked(conf, "self_autostart", "Start this VM whenever its server starts?"):
                write_other_config(x, "VM", ref, "auto_poweron", "true")
                write_other_config(x, "pool", pool_ref, "auto_poweron", "true")
                ui.ok("It starts with its server now")
        else:
            ui.ok("It starts with its server")
    else:
        ui.info("This machine isn't one of the pool's VMs (fine: it looks after it from outside).")

    # the older add-on in the servers themselves: not needed with this one
    found = []
    for ref, h in hosts.items():
        try:
            x.host.call_plugin(ref, "svm-helper", "version", {})
            found.append((ref, h["name_label"]))
        except Exception:
            pass
    if found:
        ui.warn("The Server Helper add-on is installed on %s. This VM does its work now; both would send every alert twice." % ", ".join(n for _, n in found))
        if ui.yes("Remove it from those servers?", True):
            for ref, name in found:
                try:
                    x.host.call_plugin(ref, "svm-helper", "uninstall", {})
                    ui.ok("Removed from %s" % name)
                except Exception as e:
                    ui.fail("%s: %s" % (name, short_error(e)))
    return s, x, pool_ref, pool_name, hosts, vms


def ask_url(v):
    return None if v.startswith("https://") or v.startswith("http://") else "type the whole address (https://...)"


def setup_alerts(n, total, x, pool_ref, pool_name):
    ui.step(n, total, "Where alerts go")
    settings = read_settings(x, pool_ref)
    ch = settings.setdefault("channels", {})
    current = [k for k, v in ch.items() if (v or {}).get("on")]
    if current:
        ui.ok("Already set (in the app's Helper tab, or before): %s" % ", ".join(current))
        if not ui.yes("Change where alerts go?", False):
            return settings
    names = ["Telegram (a bot sends you messages)", "ntfy (a free app; no account needed)", "Discord (a channel's webhook)", "Home Assistant (a webhook automation)", "Any other webhook (JSON)"]
    keys = ["telegram", "ntfy", "discord", "homeassistant", "webhook"]
    chosen = ui.picks("Which ones? (numbers, or none)", names, [i for i, k in enumerate(keys) if k in current])
    for i, k in enumerate(keys):
        old = ch.get(k) or {}
        if i not in chosen:
            if old:
                old["on"] = False
            continue
        if k == "telegram":
            ui.note("In Telegram, talk to @BotFather: /newbot gives you the bot's token.")
            token = ui.ask("Bot token", old.get("token", ""), check=lambda v: None if re.fullmatch(r"\d+:[A-Za-z0-9_-]{20,}", v) else "a token looks like 123456789:AAH...")
            try:
                me = ui.spin("Checking the bot", lambda: get_json("https://api.telegram.org/bot%s/getMe" % token))
                ui.ok("Bot @%s" % me.get("result", {}).get("username", "?"))
            except Exception:
                ui.warn("Telegram didn't take that token; check it later with 'svm-helper test'.")
            chat = old.get("chat", "")
            ui.note("Now send any message to your bot (or add it to a group and write there), then press Enter: this helper finds the chat by itself.")
            ui.ask("Press Enter when you've sent it", " ", keep_hint="")
            try:
                ups = get_json("https://api.telegram.org/bot%s/getUpdates" % token).get("result", [])
                chats = [u.get("message", u.get("channel_post", {})).get("chat", {}) for u in ups]
                chats = [c for c in chats if c.get("id")]
                if chats:
                    c = chats[-1]
                    chat = str(c["id"])
                    ui.ok("Found the chat %s" % (c.get("title") or c.get("first_name") or chat))
            except Exception:
                pass
            chat = ui.ask("Chat id", chat, check=lambda v: None if re.fullmatch(r"-?\d+|@\w+", v) else "a chat id is a number (or @channelname)")
            ch[k] = {"on": True, "token": token, "chat": chat}
        elif k == "ntfy":
            ui.note("Install ntfy on the phone and subscribe to the topic below. Anyone who knows a topic's name can read it: keep the random part.")
            url = ui.ask("Topic address", old.get("url") or "https://ntfy.sh/svm-" + pysecrets.token_hex(5), check=ask_url)
            ch[k] = {"on": True, "url": url, "token": old.get("token", "")}
        elif k == "discord":
            url = ui.ask("Webhook address (channel → Integrations → Webhooks)", old.get("url", ""), check=ask_url)
            ch[k] = {"on": True, "url": url}
        elif k == "homeassistant":
            url = ui.ask("Webhook address (like http://homeassistant.local:8123/api/webhook/xen-alerts)", old.get("url", ""), check=ask_url)
            ch[k] = {"on": True, "url": url}
        else:
            url = ui.ask("Webhook address", old.get("url", ""), check=ask_url)
            ch[k] = {"on": True, "url": url}
    if sends(settings):
        levels = [2, 3, 5]
        now_level = int(settings.get("min_priority", 3))
        i = ui.pick("Which alerts reach you?", ["Critical and major only", "Warnings too (recommended)", "Everything, information too"], levels.index(now_level) if now_level in levels else 1)
        settings["min_priority"] = levels[i]
    write_other_config(x, "pool", pool_ref, SETTINGS_KEY, json.dumps(settings, sort_keys=True))
    ui.ok("Saved in the pool (Simple VM Manager's Helper tab shows the same)")
    if sends(settings):
        res = ui.spin("Sending a test alert", lambda: send(settings, "Hello from the Helper VM", "Alerts from your pool will reach you here.", 5, pool_name))
        for k, v in res.items():
            (ui.ok if v == "ok" else ui.fail)("%s: %s" % (k, "reached" if v == "ok" else v))
    else:
        ui.info("No channel: alerts stay in the app and XenCenter.")
    return settings


def forget_backups(conf):
    """The backups to a NAS that earlier versions kept, gone: their settings, and the share's line
    in /etc/fstab (unmounted). What's on the NAS stays."""
    conf.pop("backups", None)
    mark = "# Helper VM backups (svm-helper setup)\n"
    try:
        with open("/etc/fstab") as f:
            lines = f.readlines()
    except OSError:
        return
    if mark not in lines:
        return
    i = lines.index(mark)
    folder = lines[i + 1].split()[1] if i + 1 < len(lines) and len(lines[i + 1].split()) > 1 else "/srv/svm-backups"
    with open("/etc/fstab", "w") as f:
        f.writelines(lines[:i] + lines[i + 2:])
    subprocess.run(["umount", "-l", folder], capture_output=True)
    subprocess.run(["systemctl", "daemon-reload"], capture_output=True)
    try:
        os.rmdir(folder)
    except OSError:
        pass
    ui.ok("Backups to the NAS are no longer done here: its share is unmounted (what's on the NAS stays)")


# ---------------------------------------------------------------- signing in to Claude from a phone

# What 'claude setup-token' shows, read from its screen. Each must be followed by something else,
# so that half of one, still coming in, isn't taken for all of it.
SIGN_IN_LINK = re.compile(r"https://[^\s\x07\x1b]*oauth[^\s\x07\x1b]*(?=[\s\x07\x1b])")
OAUTH_TOKEN = re.compile(r"sk-ant-oat[0-9]*-[A-Za-z0-9_-]{20,}(?=[^A-Za-z0-9_-])")
CODE_REFUSED = re.compile(r"(?i)\b(?:oauth )?error:[ \t]*([^\r\n]+)[\r\n]")
PAGE_KEY_LETTERS = "abcdefghijkmnpqrstuvwxyz23456789"  # none that look alike, for typing it


def screen_text(raw):
    """A full-screen program's output as plain text: its jumps along a line become spaces, and its
    other escape codes go."""
    t = re.sub(r"\x1b\[\d*[GC]", " ", raw)
    t = re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", "", t)
    t = re.sub(r"\x1b\[[0-9;?<>=]*[ -/]*[@-~]", "", t)
    return re.sub(r"\x1b[()][0-9A-Za-z]|\x1b[78=>]", "", t)


def qr_lines(text, wide=False):
    """`text` as a QR code for a terminal, drawn light where the code is light (with colours set,
    it reads the same in a light terminal). Two of its rows to a line, in half blocks; or, `wide`,
    a row to a line and two full blocks to a dot, for a Linux console, whose fonts may lack half
    blocks. [] without segno."""
    try:
        import segno
    except ImportError:
        return []
    rows = [[bool(m) for m in row] for row in segno.make_qr(text, error="m").matrix_iter(scale=1, border=2)]
    if wide:
        return ["".join("  " if m else "██" for m in row) for row in rows]
    if len(rows) % 2:
        rows.append([False] * len(rows[0]))
    glyph = {(False, False): "█", (False, True): "▀", (True, False): "▄", (True, True): " "}
    return ["".join(glyph[a, b] for a, b in zip(top, bottom)) for top, bottom in zip(rows[::2], rows[1::2])]


class SignIn:
    """'claude setup-token', run unseen in a terminal of its own: the link it gives to sign in,
    the code that sign-in ends with handed back to it, and the token it then makes."""

    def __init__(self, argv, env=None, cwd=None):
        self.argv, self.env, self.cwd = argv, env, cwd
        self.changed = threading.Condition()
        self.proc = None
        self.fd = None
        self.raw = ""
        self.ended = True
        self.run = 0

    def start(self, timeout=90):
        """A new sign-in (any earlier one stopped): its link."""
        self.stop()
        master, slave = os.openpty()
        # wide enough that the link and the token stay on one line each
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 2000, 0, 0))
        try:
            proc = subprocess.Popen(self.argv, stdin=slave, stdout=slave, stderr=slave, env=self.env, cwd=self.cwd, start_new_session=True, close_fds=True)
        except OSError:
            os.close(master)
            raise
        finally:
            os.close(slave)
        with self.changed:
            self.run += 1
            self.raw, self.ended, self.fd, self.proc = "", False, master, proc
            run = self.run
        threading.Thread(target=self._read, args=(master, run), daemon=True).start()
        found = self._wait(SIGN_IN_LINK.search, timeout)
        if not found:
            raise PoolError("Claude Code didn't give a link to sign in" + self._last())
        return found.group(0)

    def _read(self, fd, run):
        decode = codecs.getincrementaldecoder("utf-8")("replace").decode
        while True:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            with self.changed:
                if run != self.run:
                    return
                if data:
                    self.raw += decode(data)
                else:
                    self.ended = True
                self.changed.notify_all()
            if not data:
                return

    def _wait(self, look, timeout, since=0):
        """look(text shown since `since`) once it gives something; None if the program ends first,
        or the time runs out."""
        end = time.monotonic() + timeout
        with self.changed:
            while True:
                found = look(screen_text(self.raw[since:]) + ("\n" if self.ended else ""))  # nothing more comes
                left = end - time.monotonic()
                if found or self.ended or left <= 0:
                    return found
                self.changed.wait(left)

    def _last(self):
        with self.changed:
            lines = [l.strip() for l in screen_text(self.raw).splitlines()]
        lines = [l for l in lines if l and not set(l) <= set("─│╭╮╰╯ >")]
        return ": " + lines[-1][:200] if lines else ""

    def answer(self, given, timeout=90):
        """Hands the sign-in the code it ended with: ("token", the token), or ("refused", why),
        after which a new sign-in has to start."""
        with self.changed:
            since, fd, ended = len(self.raw), self.fd, self.ended
        if ended or fd is None:
            return "refused", "that sign-in had ended"
        try:
            os.write(fd, given.encode())
            time.sleep(0.3)
            os.write(fd, b"\r")
        except OSError:
            return "refused", "Claude Code had stopped"

        def look(t):
            m = OAUTH_TOKEN.search(t)
            if m:
                return "token", m.group(0)
            m = CODE_REFUSED.search(t)
            return ("refused", m.group(1).strip()) if m else None

        return self._wait(look, timeout, since) or ("refused", ("Claude Code stopped" if self.ended else "Claude didn't answer in time") + self._last())

    def stop(self):
        with self.changed:
            self.run += 1
            proc, fd = self.proc, self.fd
            self.proc, self.fd, self.ended = None, None, True
            self.changed.notify_all()
        if proc and proc.poll() is None:
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.killpg(proc.pid, sig)
                    proc.wait(5)
                    break
                except (OSError, subprocess.TimeoutExpired):
                    pass
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass


PAGE_WORDS = {
    "en": {
        "title": "Link the Helper VM to Claude",
        "step1": "1. Sign in to Claude and approve.",
        "button": "Sign in to Claude",
        "hint": "It opens in a new tab: come back to this one after.",
        "step2": "2. Claude then shows a code. Copy it and paste it here:",
        "send": "Send",
        "sending": "Checking…",
        "done": "Done. The Helper VM now uses your Claude subscription. You can close this page.",
        "refused": "Claude didn't accept that code (it may be incomplete, or from an earlier sign-in). Sign in again with the button below, and paste the new code.",
        "empty": "Paste the code first.",
        "broken": "The sign-in couldn't start again. Look at the Helper VM's terminal.",
    },
    "pt": {
        "title": "Ligar a VM helper ao Claude",
        "step1": "1. Entre no Claude e aprove.",
        "button": "Entrar no Claude",
        "hint": "Abre numa aba nova: depois volte para esta.",
        "step2": "2. Depois o Claude mostra um código. Copie e cole aqui:",
        "send": "Enviar",
        "sending": "Conferindo…",
        "done": "Pronto. A VM helper já usa a sua assinatura do Claude. Pode fechar esta página.",
        "refused": "O Claude não aceitou esse código (pode estar incompleto, ou ser de um login anterior). Entre de novo pelo botão abaixo e cole o código novo.",
        "empty": "Cole o código primeiro.",
        "broken": "Não deu para começar o login de novo. Veja o terminal da VM helper.",
    },
    "es": {
        "title": "Conectar la VM auxiliar a Claude",
        "step1": "1. Inicia sesión en Claude y aprueba.",
        "button": "Iniciar sesión en Claude",
        "hint": "Se abre en una pestaña nueva: luego vuelve a esta.",
        "step2": "2. Luego Claude muestra un código. Cópialo y pégalo aquí:",
        "send": "Enviar",
        "sending": "Comprobando…",
        "done": "Listo. La VM auxiliar ya usa tu suscripción de Claude. Puedes cerrar esta página.",
        "refused": "Claude no aceptó ese código (puede estar incompleto, o ser de un inicio de sesión anterior). Vuelve a iniciar sesión con el botón de abajo y pega el código nuevo.",
        "empty": "Primero pega el código.",
        "broken": "No se pudo iniciar la sesión de nuevo. Mira la terminal de la VM auxiliar.",
    },
}


def page_language(accept):
    """The page's language, from the browser's Accept-Language."""
    for part in (accept or "").lower().split(","):
        tag = part.split(";")[0].strip()[:2]
        if tag in PAGE_WORDS:
            return tag
    return "en"


def phone_page(lang, link, said):
    w, e = PAGE_WORDS[lang], html.escape
    if said == "done":
        body = '<p class="ok">%s</p>' % e(w["done"])
    else:
        body = ('<p class="bad">%s</p>' % e(w[said]) if said in ("refused", "empty", "broken") else "") + (
            '<p>%s</p><a class="button" href="%s" target="_blank" rel="noopener noreferrer">%s</a><p class="hint">%s</p>'
            '<form method="post" onsubmit="var b=this.querySelector(\'button\');b.disabled=true;b.textContent=\'%s\'">'
            '<p>%s</p><input name="code" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" required>'
            '<button type="submit">%s</button></form>'
        ) % (e(w["step1"]), e(link), e(w["button"]), e(w["hint"]), e(w["sending"]), e(w["step2"]), e(w["send"]))
    return """<!doctype html>
<html lang="%s"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>%s</title><style>
:root{--bg:#f6f7f9;--card:#fff;--text:#1d2330;--dim:#5d6675;--accent:#2f6fde;--ok:#1f8a4c;--bad:#c2412d;--line:#d9dde4}
@media (prefers-color-scheme:dark){:root{--bg:#15181e;--card:#1e222a;--text:#e7e9ee;--dim:#9aa3b2;--accent:#5b8ff0;--ok:#4cc27e;--bad:#f07a63;--line:#343a46}}
body{margin:0;background:var(--bg);color:var(--text);font:17px/1.45 system-ui,-apple-system,"Segoe UI",sans-serif}
main{max-width:28rem;margin:0 auto;padding:24px 16px}
h1{font-size:1.3rem;margin:0 0 20px}
.button,button{display:block;box-sizing:border-box;width:100%%;padding:14px;border:0;border-radius:10px;background:var(--accent);color:#fff;font:inherit;font-weight:600;text-align:center;text-decoration:none}
button:disabled{opacity:.6}
input{box-sizing:border-box;width:100%%;padding:12px;margin:0 0 12px;border:1px solid var(--line);border-radius:10px;background:var(--card);color:var(--text);font:15px ui-monospace,monospace}
.hint{color:var(--dim);font-size:.9rem;margin:8px 0 28px}
.ok{color:var(--ok);font-weight:600}.bad{color:var(--bad)}
</style></head><body><main><h1>%s</h1>%s</main></body></html>
""" % (lang, e(w["title"]), e(w["title"]), body)


class PhonePage:
    """A page this VM serves while setup signs in to Claude, at a link hard to guess, for a phone
    on the same network: a button to Claude's sign-in, and a box for the code it ends with. The
    token stays here: the page only says whether it worked."""

    def __init__(self, sign_in, link, address="0.0.0.0", port=0):
        self.sign_in, self.link = sign_in, link
        self.key = "".join(pysecrets.choice(PAGE_KEY_LETTERS) for _ in range(8))
        self.token = ""
        self.failed = ""
        self.status = "Waiting for your phone (Ctrl+C stops)"
        self.notes = []  # what the terminal should say, as it happens
        self.done = threading.Event()
        self.lock = threading.Lock()  # one code at a time
        page = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                page.serve(self, None)

            def do_POST(self):
                try:
                    n = max(0, min(int(self.headers.get("Content-Length") or 0), 8192))
                except ValueError:
                    n = 0
                form = urllib.parse.parse_qs(self.rfile.read(n).decode("utf-8", "replace"))
                page.serve(self, (form.get("code") or [""])[0])

        self.server = http.server.ThreadingHTTPServer((address, port), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def serve(self, req, given):
        if req.path.split("?")[0].lower() != "/" + self.key:  # the QR code has it in capitals
            req.send_response(404)
            req.send_header("Content-Length", "0")
            req.end_headers()
            return
        said = self.take(given.strip()) if given is not None else ("done" if self.token else "")
        body = phone_page(page_language(req.headers.get("Accept-Language")), self.link, said).encode()
        req.send_response(200)
        for name, value in (("Content-Type", "text/html; charset=utf-8"), ("Content-Length", str(len(body))), ("Cache-Control", "no-store"), ("Referrer-Policy", "no-referrer")):
            req.send_header(name, value)
        req.end_headers()
        req.wfile.write(body)

    def take(self, given):
        """A code from the phone: "done", or what the page should say."""
        with self.lock:
            if self.done.is_set():
                return "done" if self.token else "broken"
            if not given:
                return "empty"
            self.status = "Checking the code from your phone"
            got, why = self.sign_in.answer(given)
            if got == "token":
                self.token = why
                self.done.set()
                return "done"
            self.notes.append("The code from the phone didn't work (Claude: %s) The phone's page has a new sign-in now." % why.rstrip("."))
            self.status = "Waiting for your phone (Ctrl+C stops)"
            try:
                self.link = self.sign_in.start()
            except Exception as e:
                self.failed = "the sign-in couldn't start again: " + short_error(e)
                self.done.set()
                return "broken"
            return "refused"

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def let_in(port):
    """Lets the phone reach `port` while it signs in, when this VM's firewall would stop it: the
    function that closes it again."""
    def run(*argv):
        try:
            return subprocess.run(list(argv), capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            return None

    r = run("firewall-cmd", "--state") if shutil.which("firewall-cmd") else None
    if r and r.returncode == 0:
        run("firewall-cmd", "--add-port=%d/tcp" % port)  # until firewalld reloads, at most
        return lambda: run("firewall-cmd", "--remove-port=%d/tcp" % port)
    r = run("ufw", "status") if shutil.which("ufw") else None
    if r and r.returncode == 0 and "Status: active" in r.stdout:
        run("ufw", "allow", "%d/tcp" % port)
        return lambda: run("ufw", "delete", "allow", "%d/tcp" % port)
    return lambda: None


def wait_for_phone(page, timeout=1800):
    """The token, once the phone's sign-in ends with it (a spinner meanwhile)."""
    end, shown, i = time.monotonic() + timeout, None, 0
    try:
        while not page.done.wait(0.1):
            while page.notes:
                if ui.color:
                    sys.stdout.write("\r\033[K")
                ui.warn(page.notes.pop(0))
            if time.monotonic() > end:
                raise PoolError("nothing came from the phone in %d minutes" % (timeout // 60))
            if ui.color:
                sys.stdout.write("\r\033[K        " + ui.cyan(ui.frames[i % len(ui.frames)]) + " " + page.status)
                sys.stdout.flush()
            elif page.status != shown:
                ui.info(page.status)
            shown, i = page.status, i + 1
    finally:
        if ui.color:
            sys.stdout.write("\r\033[K")
            sys.stdout.flush()
    if not page.token:
        raise PoolError(page.failed or "the sign-in didn't finish")
    ui.ok("Signed in to Claude: the helper has its token")
    return page.token


def phone_sign_in(conf):
    """The subscription's token, from a sign-in done on a phone: a QR code here opens a page this
    VM serves, with Claude's sign-in; the code that sign-in gives comes back through the page, and
    Claude Code, run here unseen, turns it into the token."""
    home = tempfile.mkdtemp(prefix="svm-claude-setup-")
    no_browser = os.path.join(home, "no-browser")  # nothing to open here: the phone does it
    os.makedirs(no_browser)
    for name in ("xdg-open", "sensible-browser", "x-www-browser", "www-browser"):
        with open(os.path.join(no_browser, name), "w") as f:
            f.write("#!/bin/sh\nexit 0\n")
        os.chmod(os.path.join(no_browser, name), 0o755)
    u = pwd.getpwnam(USER)
    os.chown(home, u.pw_uid, u.pw_gid)
    env = ["HOME=" + home, "PATH=%s:/usr/local/bin:/usr/bin:/bin" % no_browser, "TERM=xterm-256color", "LANG=C.UTF-8", "DISABLE_AUTOUPDATER=1"]
    sign_in = SignIn([shutil.which("runuser") or "/usr/sbin/runuser", "-u", USER, "--", "env", "-i"] + env + [claude_bin(), "setup-token"], cwd=home)
    page, close = None, None
    try:
        link = ui.spin("Starting Claude's sign-in", sign_in.start)
        towards = split_port(((conf.get("pool") or {}).get("hosts") or ["1.1.1.1"])[0])[0]
        address = my_address(towards) or my_address("1.1.1.1")
        if not address:
            raise PoolError("this VM has no network address for the phone to reach")
        page = PhonePage(sign_in, link)
        close = let_in(page.port)
        url = "http://%s:%d/%s" % (address, page.port, page.key)
        # in capitals, the QR code is smaller (it then takes its letters-and-digits mode)
        lines = qr_lines(url.upper(), wide=ui.console)
        print()
        if lines:
            ui.info("Point your phone's camera at this code (the phone on the same network as this VM):")
            print()
            for line in lines:
                print("        " + ui.c("37;40", line))
            print()
            if shutil.get_terminal_size().lines < len(lines) + 6:
                ui.warn("The code may not fit on this screen: if it doesn't scan, type the link below.")
            ui.info("Or open " + ui.bold(url))
        else:
            ui.info("On your phone (on the same network as this VM), open " + ui.bold(url))
        ui.note("The page has a button to sign in to Claude and approve. Claude then shows a code: copy it into the page's box and send it. The token stays on this VM.")
        print()
        return wait_for_phone(page)
    finally:
        if page:
            page.close()
        if close:
            close()
        sign_in.stop()
        shutil.rmtree(home, ignore_errors=True)


def install_claude_code():
    """Claude Code installed (or brought up to date) for the helper's user, in its own folder."""
    os.makedirs(CLAUDE_HOME, exist_ok=True)
    u = pwd.getpwnam(USER)
    os.chown(CLAUDE_HOME, u.pw_uid, u.pw_gid)
    subprocess.run(["runuser", "-u", USER, "--", "env", "HOME=" + CLAUDE_HOME, "bash", "-c", "curl -fsSL https://claude.ai/install.sh | bash"], check=True, capture_output=True, timeout=900, cwd=CLAUDE_HOME)
    if not claude_bin():
        raise PoolError("it installed, but its command isn't where it should be")


def subscription_token(sec, conf):
    """The token that lets the helper use the owner's Claude subscription: the one kept, one from a
    sign-in done on a phone (a QR code here), or one pasted, made with 'claude setup-token' on
    another computer. "" for none."""
    if sec.get("claude_oauth_token") and ui.yes("Keep the subscription token saved before?", True):
        return sec["claude_oauth_token"]
    ui.note("The helper uses your subscription with a token from Claude Code, made once: you sign in to Claude and approve it. The easy way is your phone: this shows a QR code, your camera opens a page on this VM (only on your own network), and you sign in there. Or make the token on any computer with Claude Code ('claude setup-token') and paste it here.")
    while True:
        if ui.pick("How do you want to sign in?", ["With your phone: a QR code (recommended)", "Paste a token from 'claude setup-token' on another computer"], 0) == 1:
            break
        try:
            return phone_sign_in(conf)
        except KeyboardInterrupt:
            print()
            ui.warn("Stopped.")
        except Exception as e:
            ui.fail("The sign-in didn't work: " + short_error(e))
    return ui.ask("Paste the token (Enter: no Claude for now)", "", secret=True, check=lambda v: None if not v or (v.startswith("sk-ant-") and len(v) > 30) else "the token starts with sk-ant-")


def setup_claude(n, total, conf, sec):
    ui.step(n, total, "Claude")
    ui.note("Claude explains serious alerts in plain words when they reach your phone, and writes a weekly health review of the pool. With your Claude subscription (Pro or Max) it costs nothing more: the helper uses it through Claude Code, and it counts in your plan's usage like your own chats do (a review a week, a few alerts). An API key is billed per use instead. Without Claude, the review is a plain summary.")
    c = conf["claude"]
    modes = ["subscription", "api", "off"]
    now = Claude(conf, sec).mode
    i = ui.pick("How should the helper use Claude?", ["Your Claude subscription: no extra cost (recommended)", "An Anthropic API key: billed per use", "Not at all"], modes.index(now) if now in modes else 0)
    c["auth"] = modes[i]
    if c["auth"] == "subscription":
        try:
            ui.spin("Installing Claude Code for the helper" if not claude_bin() else "Bringing Claude Code up to date", install_claude_code)
        except Exception:
            ui.note("Without Claude Code the subscription can't be used: try 'sudo svm-helper setup' again later, or choose the API key.")
            c["auth"] = "off"
    if c["auth"] == "subscription":
        token = subscription_token(sec, conf)
        if not token:
            ui.note("Without a token Claude stays off for now: 'sudo svm-helper setup' asks again.")
            c["auth"] = "off"
    if c["auth"] == "subscription":
        try:
            ui.spin("Asking Claude a first question", lambda: Claude(conf, {"claude_oauth_token": token}).check())
        except Exception:
            ui.note("The token was kept anyway: 'sudo svm-helper setup' gives another.")
        sec["claude_oauth_token"] = token
        sec.pop("anthropic_api_key", None)
    elif c["auth"] == "api":
        had = bool(sec.get("anthropic_api_key"))
        key = ui.ask("API key (console.anthropic.com → API keys)", sec.get("anthropic_api_key", ""), secret=True, keep_hint="Enter keeps the saved one" if had else "", check=lambda v: None if v.startswith("sk-ant-") else "the key starts with sk-ant-")
        if key != sec.get("anthropic_api_key"):
            try:
                ui.spin("Checking the key", lambda: Claude(dict(conf, claude=dict(c, auth="api")), {"anthropic_api_key": key}).check())
            except Exception:
                ui.note("It was kept anyway: 'sudo svm-helper setup' gives another.")
        sec["anthropic_api_key"] = key
        sec.pop("claude_oauth_token", None)
    else:
        sec.pop("anthropic_api_key", None)
        sec.pop("claude_oauth_token", None)
    uses = c["auth"] != "off"
    langs = list(LANGUAGES)
    i = ui.pick("Language for its messages", ["Português (Brasil)", "English", "Español"], langs.index(conf.get("language")) if conf.get("language") in langs else 0)
    conf["language"] = langs[i]
    if uses:
        c["explain"] = ui.yes("Explain serious alerts (critical and major) with Claude?", c.get("explain", True))
    rv = c.setdefault("review", {})
    rv["on"] = ui.yes("Send a weekly health review of the pool?", rv.get("on", True))
    if rv["on"]:
        day = ui.ask("On which day", rv.get("day", "Monday")[:3].lower(), check=lambda v: None if v[:3].title() in [d[:3] for d in DAYS] else "a day, like mon")
        rv["day"] = next(d for d in DAYS if d[:3] == day[:3].title())
        rv["at"] = ui.ask("At what time", rv.get("at", "08:00"), check=lambda v: None if re.fullmatch(r"([01]?\d|2[0-3]):[0-5]\d", v) else "type it like 08:00")


def setup_claude_code(conf, user, pool_name, hosts):
    """Claude Code for the person who ran setup, with a CLAUDE.md that tells it about this machine."""
    if not user or user == "root":
        ui.info("Claude Code: run setup with sudo from your own user to have it installed for you.")
        return
    ui.note("Claude Code is Anthropic's assistant for the terminal. Here it can look after the pool with you: 'how full is the NAS?', 'which VMs have no snapshot schedule?', 'restart servARR'. It asks before it runs anything, and logs in with your Claude account the first time.")
    if not asked(conf, "claude_code", "Install Claude Code for %s?" % user):
        return
    home = pwd.getpwnam(user).pw_dir
    if not os.path.exists(os.path.join(home, ".local", "bin", "claude")):
        try:
            ui.spin("Installing Claude Code", lambda: subprocess.run(["runuser", "-l", user, "-c", "curl -fsSL https://claude.ai/install.sh | bash"], check=True, capture_output=True, timeout=600))
        except Exception:
            ui.note("You can install it later: curl -fsSL https://claude.ai/install.sh | bash")
            return
    else:
        ui.ok("Claude Code is already installed")
    work = os.path.join(home, "pool")
    os.makedirs(work, exist_ok=True)
    template = os.path.join(APP_DIR, "CLAUDE.md")
    text = open(template).read() if os.path.exists(template) else "# The pool\n"
    text += "\n## This pool (written by svm-helper setup)\n\n- Name: %s\n" % pool_name
    for h in hosts.values():
        text += "- Server %s at %s\n" % (h.get("name_label"), h.get("address"))
    with open(os.path.join(work, "CLAUDE.md"), "w") as f:
        f.write(text)
    u = pwd.getpwnam(user)
    for p in (work, os.path.join(work, "CLAUDE.md")):
        os.chown(p, u.pw_uid, u.pw_gid)
    ui.ok("~/pool/CLAUDE.md tells it about this machine and the pool")
    in_group = user in grp.getgrnam(USER).gr_mem
    if not in_group:
        ui.note("For Claude Code (and you) to use 'svm-helper' without sudo, %s can join the group %s. That lets %s read the pool's password in /etc/svm-helper: fine on a machine that's only for this." % (user, USER, user))
        if asked(conf, "claude_code_group", "Add %s to the group %s?" % (user, USER)):
            subprocess.run(["usermod", "-aG", USER, user], capture_output=True)
            ui.ok("Done (it takes effect at %s's next login)" % user)
    ui.ok("To start: %s" % ui.bold("cd ~/pool && claude"))


def cmd_setup(args):
    if os.geteuid() != 0:
        die("Run it as root: sudo svm-helper setup")
    from_installer = "--from-installer" in args
    base = 2 if from_installer else 0
    total = base + 4
    if not from_installer:
        ui.banner("Helper VM %s · setup" % VERSION, "Simple VM Manager's helper for your XenServer pool")
        ui.note("Enter keeps what's in brackets. Run it again any time to change an answer.")
    conf, sec = load_config(), load_secrets()
    session = None
    try:
        session, x, pool_ref, pool_name, hosts, vms = setup_pool(base + 1, total, conf, sec)
        save_conf(conf, sec)
        setup_alerts(base + 2, total, x, pool_ref, pool_name)
        forget_backups(conf)
        save_conf(conf, sec)
        setup_claude(base + 3, total, conf, sec)
        save_conf(conf, sec)
        setup_claude_code(conf, os.environ.get("SUDO_USER", ""), pool_name, hosts)
        save_conf(conf, sec)
        ui.step(base + 4, total, "Starting it")
        os.makedirs(STATE_DIR, exist_ok=True)
        subprocess.run(["systemctl", "enable", SERVICE], capture_output=True)
        t0 = time.time()
        restart_service()

        def first_look():
            for _ in range(120):
                st = load_json(state_path("state"), {})
                if st.get("last_look", 0) >= t0 or st.get("unreachable_since"):
                    return st
                time.sleep(1)
            raise PoolError("it hasn't looked yet: see 'journalctl -u svm-helper'")

        try:
            ui.spin("Its first look at the pool", first_look)
        except Exception:
            pass
        ui.ok("svm-helper runs now, and starts with this machine")
    except KeyboardInterrupt:
        print()
        if (conf.get("pool") or {}).get("hosts"):
            save_conf(conf, sec)
        die("Stopped. What was answered is kept; run 'sudo svm-helper setup' to go on.")
    except Exception as e:
        try:
            with open(os.path.join(STATE_DIR, "setup-error.log"), "a") as f:
                f.write(time.strftime("%Y-%m-%d %H:%M:%S ") + traceback.format_exc() + "\n")
        except OSError:
            pass
        die("Setup stopped: %s (details in %s/setup-error.log)" % (short_error(e), STATE_DIR))
    finally:
        if session is not None:
            try:
                session.xenapi.session.logout()
            except Exception:
                pass
    rv = conf["claude"].get("review") or {}
    lines = [
        ui.bold("The helper looks after '%s' now." % pool_name),
        "",
        "Claude:   " + (Claude(conf, sec).how() if Claude(conf, sec).ready() else "off"),
        "Review:   " + (("%ss at %s, %s" % (rv.get("day"), rv.get("at"), "by Claude" if Claude(conf, sec).ready() else "a plain summary")) if rv.get("on") else "off"),
        "",
        ui.cyan("svm-helper status") + "        how it's doing, what it did lately",
        ui.cyan("svm-helper check") + "         one look now",
        ui.cyan("svm-helper test") + "          a test alert to your phone",
        ui.cyan("svm-helper review") + "        this week's review, now",
        ui.cyan("svm-helper advise") + "        the Advisor's checklist, now",
        ui.cyan("sudo svm-helper setup") + "    change these answers",
        ui.cyan("journalctl -u svm-helper -f") + "  its log, live",
    ]
    ui.box("Done", lines)
    print()


# ---------------------------------------------------------------- main

COMMANDS = {
    "run": (lambda a: Helper().run(), "the service (systemd runs it)"),
    "status": (cmd_status, "how it's doing, what it did lately"),
    "check": (cmd_check, "one look at the pool now"),
    "test": (cmd_test, "a test alert to your channels"),
    "review": (cmd_review, "the weekly review now (--send sends it too)"),
    "advise": (cmd_advise, "the Advisor's checklist of good practice, now"),
    "xapi": (cmd_xapi, "one Python expression on the pool's API (x = session.xenapi)"),
    "shell": (cmd_shell, "a Python shell on the pool's API"),
    "trust": (cmd_trust, "read and pin the servers' certificates again"),
    "setup": (cmd_setup, "the questions (as root)"),
    "version": (lambda a: print(VERSION), "its version"),
}


def drop_privileges():
    """Run as the helper's own user (as root, the files it writes would be root's)."""
    try:
        u = pwd.getpwnam(USER)
    except KeyError:
        return
    os.setgroups([])
    os.setgid(u.pw_gid)
    os.setuid(u.pw_uid)


def main(argv):
    os.umask(0o007)
    cmd = argv[1] if len(argv) > 1 else "help"
    if cmd in ("help", "-h", "--help") or cmd not in COMMANDS:
        ui.banner("Helper VM %s" % VERSION, "Simple VM Manager's helper for your XenServer pool")
        print()
        for k, (_, what) in COMMANDS.items():
            print("    " + ui.cyan(k.ljust(9)) + " " + what)
        print()
        return 0 if cmd in ("help", "-h", "--help") else 1
    if cmd not in ("setup", "trust", "version") and os.geteuid() == 0:
        drop_privileges()
    if cmd not in ("setup", "trust", "version", "run") and not os.access(SECRETS, os.R_OK) and os.path.exists(SECRETS):
        die("Run it with sudo (or join the group %s)." % USER)
    try:
        COMMANDS[cmd][0](argv[2:])
    except KeyboardInterrupt:
        print()
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
