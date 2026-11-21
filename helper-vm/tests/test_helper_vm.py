"""Tests of the Helper VM (helper-vm/svm-helper.py): its logic against a pretend pool, and its
connection against a pretend XAPI server over HTTPS. Run: python3 helper-vm/tests/test_helper_vm.py
(the connection tests need the XenAPI package and openssl; they're skipped without them)."""

import calendar
import datetime
import hashlib
import http.server
import importlib.machinery
import importlib.util
import json
import os
import shutil
import socketserver
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request
import xmlrpc.client
import xmlrpc.server

HERE = os.path.dirname(os.path.abspath(__file__))
TMP = tempfile.mkdtemp(prefix="svm-helper-test-")
os.environ["SVM_HELPER_STATE"] = os.path.join(TMP, "state")
os.environ["SVM_HELPER_CONF"] = os.path.join(TMP, "conf")
os.makedirs(os.environ["SVM_HELPER_STATE"])
os.makedirs(os.environ["SVM_HELPER_CONF"])
sys.dont_write_bytecode = True
loader = importlib.machinery.SourceFileLoader("svm_helper_vm", os.path.join(HERE, "..", "svm-helper.py"))
spec = importlib.util.spec_from_loader("svm_helper_vm", loader)
h = importlib.util.module_from_spec(spec)
loader.exec_module(h)

HAVE_XENAPI = importlib.util.find_spec("XenAPI") is not None
HAVE_ANTHROPIC = importlib.util.find_spec("anthropic") is not None


def xd(epoch):
    return time.strftime("%Y%m%dT%H:%M:%SZ", time.gmtime(epoch))


class NS:
    pass


class FakePool:
    """A pretend pool: what XenAPI would answer, kept in dicts."""

    def __init__(self):
        now = time.time()
        self.messages = []
        self.plugged = []
        self.plugin_calls = []
        self.other_config = {}
        self.hosts = {
            "H1": {"uuid": "u-h1", "name_label": "tico", "address": "127.0.0.1", "enabled": True, "metrics": "M1", "other_config": {"boot_time": str(now - 3600)}, "software_version": {"product_brand": "XenServer", "product_version": "8.4"}},
            "H2": {"uuid": "u-h2", "name_label": "godofredo", "address": "127.0.0.2", "enabled": True, "metrics": "M2", "other_config": {"boot_time": str(now - 600)}, "software_version": {}},
        }
        self.srs = {
            "S1": {"uuid": "u-s1", "name_label": "Local storage", "type": "ext", "content_type": "user", "shared": False, "physical_size": str(100 * 1024 ** 3), "physical_utilisation": str(95 * 1024 ** 3), "other_config": {}},
            "S2": {"uuid": "u-s2", "name_label": "TrueNAS", "type": "nfs", "content_type": "user", "shared": True, "physical_size": str(1000 * 1024 ** 3), "physical_utilisation": str(10 * 1024 ** 3), "other_config": {}},
        }
        self.pbds = {
            "P1": {"uuid": "u-p1", "SR": "S1", "host": "H1", "currently_attached": True},
            "P2": {"uuid": "u-p2", "SR": "S2", "host": "H1", "currently_attached": True},
            "P3": {"uuid": "u-p3", "SR": "S2", "host": "H2", "currently_attached": False},
        }
        self.vmss = {}
        self.server_local_offset = -3 * 3600
        o = self
        x = NS()
        x.pool = NS()
        x.pool.get_all = lambda: ["POOL"]
        x.pool.get_record = lambda p: {"name_label": "home", "uuid": "u-pool", "master": "H1"}
        x.pool.get_master = lambda p: "H1"
        x.pool.get_uuid = lambda p: "u-pool"
        x.pool.get_name_label = lambda p: "home"
        x.pool.get_other_config = lambda p: o.other_config
        x.pool.remove_from_other_config = lambda p, k: o.other_config.pop(k, None)
        x.pool.add_to_other_config = lambda p, k, v: o.other_config.__setitem__(k, v)
        x.host = NS()
        x.host.get_all_records = lambda: o.hosts
        x.host.get_servertime = lambda r: xmlrpc.client.DateTime(xd(time.time()))
        x.host.get_server_localtime = lambda r: xmlrpc.client.DateTime(time.strftime("%Y%m%dT%H:%M:%S", time.gmtime(time.time() + o.server_local_offset)))
        x.host.call_plugin = lambda host, plugin, fn, args: o.plugin_calls.append((plugin, fn, args)) or "True"
        x.host.get_name_label = lambda r: o.hosts[r]["name_label"]
        x.host.get_by_uuid = lambda u: [k for k, v in o.hosts.items() if v["uuid"] == u][0]
        x.host_metrics = NS()
        x.host_metrics.get_all_records = lambda: {"M1": {"live": True}, "M2": {"live": True}}
        x.SR = NS()
        x.SR.get_all_records = lambda: o.srs
        x.SR.get_name_label = lambda r: o.srs[r]["name_label"]
        x.SR.get_by_uuid = lambda u: [k for k, v in o.srs.items() if v["uuid"] == u][0]
        x.PBD = NS()
        x.PBD.get_all_records = lambda: o.pbds
        x.PBD.plug = lambda r: o.plugged.append(r)
        x.SM = NS()
        x.SM.get_all_records = lambda: {}
        x.VMSS = NS()
        x.VMSS.get_all_records = lambda: o.vmss
        x.VMSS.get_name_label = lambda r: o.vmss[r]["name_label"]
        x.VMSS.get_by_uuid = lambda u: [k for k, v in o.vmss.items() if v["uuid"] == u][0]
        x.message = NS()
        x.message.create = lambda name, prio, cls, uuid, body: o.messages.append({"name": name, "priority": prio, "cls": cls, "obj_uuid": uuid, "body": body, "timestamp": xmlrpc.client.DateTime(xd(time.time()))})
        x.message.get_since = lambda since: {str(i): m for i, m in enumerate(o.messages) if h.xen_epoch(m["timestamp"]) >= h.xen_epoch(since)}
        self.x = x


class FakeConn:
    def __init__(self, x):
        self.x = x
        self.address = "127.0.0.1"


def look_at(fp, conf=None, state=None, now=None):
    return h.Look(FakeConn(fp.x), h.merged(h.DEFAULT_CONFIG, conf or {}), {}, state if state is not None else {}, now or time.time())


class Hook(http.server.BaseHTTPRequestHandler):
    got = []

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        Hook.got.append(json.loads(self.rfile.read(n)))
        self.send_response(200)
        self.end_headers()

    def do_GET(self):
        self.send_response(200 if self.path == "/up" else 503)
        self.end_headers()

    def log_message(self, *a):
        pass


class Quiet(socketserver.ThreadingTCPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        pass  # (a certificate read hangs up halfway, by design)


def serve(handler, context=None):
    srv = Quiet(("127.0.0.1", 0), handler)
    if context:
        srv.socket = context.wrap_socket(srv.socket, server_side=True)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, srv.server_address[1]


class Source(http.server.BaseHTTPRequestHandler):
    """An ESXi datastore's flat file, behind basic auth (root:secret)."""

    data = bytes(range(256)) * 4096

    def do_GET(self):
        if self.headers.get("Authorization") != "Basic cm9vdDpzZWNyZXQ=":
            self.send_response(401)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(Source.data)))
        self.end_headers()
        self.wfile.write(Source.data)

    def log_message(self, *a):
        pass


class Copies(unittest.TestCase):
    """The disk copies Simple VM Manager hands over (VM Conversion)."""

    def test_an_http_source(self):
        srv, port = serve(Source)
        try:
            job = {"kind": "http", "url": "http://127.0.0.1:%d/folder/dc01/dc01-flat.vmdk?dcPath=ha-datacenter&dsName=datastore1" % port, "user": "root", "size": len(Source.data)}
            src, size, end = h.open_source(job, "secret")
            self.assertEqual(size, len(Source.data))
            self.assertEqual(src.read(size), Source.data)
            end()
            with self.assertRaises(h.PoolError) as e:
                h.open_source(job, "wrong")
            self.assertIn("401", str(e.exception))
            with self.assertRaises(h.PoolError) as e:
                h.open_source(dict(job, size=5), "secret")
            self.assertIn("for a disk of 5", str(e.exception))
        finally:
            srv.shutdown()

    def test_an_ssh_source(self):
        # a pretend ssh: it asks SSH_ASKPASS for the password, as the real one does, then runs
        # the command here
        bin_dir = os.path.join(TMP, "bin")
        os.makedirs(bin_dir, exist_ok=True)
        fake = os.path.join(bin_dir, "ssh")
        with open(fake, "w") as f:
            f.write('#!/bin/sh\n[ "$("$SSH_ASKPASS")" = secret ] || { echo "root@pve: Permission denied (password)." >&2; exit 255; }\nfor last; do :; done\nexec sh -c "$last"\n')
        os.chmod(fake, 0o755)
        old = os.environ["PATH"]
        os.environ["PATH"] = bin_dir + os.pathsep + old
        try:
            job = {"kind": "ssh", "host": "pve.lan", "port": 22, "user": "root", "command": "printf '5\\nhello'", "size": 5}
            src, size, end = h.open_source(job, "secret")
            self.assertEqual((size, src.read(5)), (5, b"hello"))
            end()
            with self.assertRaises(h.PoolError) as e:
                h.open_source(job, "wrong")
            self.assertIn("Permission denied", str(e.exception))
        finally:
            os.environ["PATH"] = old

    def test_each_copy_is_claimed_once(self):
        tasks = {
            "T1": {"status": "pending", "other_config": {"svm-copy": json.dumps({"kind": "http"})}},
            "T2": {"status": "pending", "other_config": {"svm-copy": "{}", "svm-copy-claimed": "other"}},
            "T3": {"status": "success", "other_config": {"svm-copy": "{}"}},
            "T4": {"status": "pending", "other_config": {}},
        }

        def add(ref, k, v):
            if k in tasks[ref]["other_config"]:
                raise Exception("MAP_DUPLICATE_KEY")
            tasks[ref]["other_config"][k] = v

        x = NS()
        x.task = NS()
        x.task.get_all_records = lambda: tasks
        x.task.add_to_other_config = add
        got = h.claim_copies(x, "svm-helper", {})
        self.assertEqual(got, [("T1", {"kind": "http"})])
        self.assertEqual(tasks["T1"]["other_config"]["svm-copy-claimed"], "svm-helper")
        self.assertEqual(h.claim_copies(x, "another", {}), [])


class Logic(unittest.TestCase):
    def test_last_expected_on_the_pool_clock(self):
        # a daily run at 19:45 on a pool 3 hours behind UTC is due at 22:45 UTC
        now = calendar.timegm((2026, 11, 21, 23, 30, 0))
        self.assertEqual(h.last_expected("daily", {"hour": "19", "min": "45"}, now, -3 * 3600), calendar.timegm((2026, 11, 21, 22, 45, 0)))
        # before it: yesterday's
        now = calendar.timegm((2026, 11, 21, 22, 0, 0))
        self.assertEqual(h.last_expected("daily", {"hour": "19", "min": "45"}, now, -3 * 3600), calendar.timegm((2026, 11, 20, 22, 45, 0)))
        # weekly: 2026-11-21 is a Saturday
        now = calendar.timegm((2026, 11, 21, 12, 0, 0))
        self.assertEqual(h.last_expected("weekly", {"hour": "1", "min": "0", "days": "Wednesday"}, now, 0), calendar.timegm((2026, 11, 18, 1, 0, 0)))
        self.assertEqual(h.last_expected("hourly", {"min": "30"}, now, 0), calendar.timegm((2026, 11, 21, 11, 30, 0)))

    def test_due(self):
        t = datetime.datetime(2026, 11, 23, 9, 0)  # a Monday, 09:00 local
        now = time.mktime(t.timetuple())
        slot = time.mktime(t.replace(hour=8).timetuple())
        self.assertTrue(h.due("08:00", ["Monday"], 0, now))
        self.assertFalse(h.due("08:00", ["Monday"], slot + 5, now))  # ran already
        self.assertFalse(h.due("08:00", ["Tuesday"], 0, now))
        self.assertFalse(h.due("02:30", [], 0, now))  # 6.5 h late: let go
        self.assertTrue(h.due("08:30", [], 0, now))
        self.assertFalse(h.due("bad", [], 0, now))

    def test_words(self):
        self.assertEqual(h.span(40), "40 s")
        self.assertEqual(h.span(12 * 60), "12 min")
        self.assertEqual(h.span(3 * 3600 + 300), "3 h 5 min")
        self.assertEqual(h.span(5 * 86400), "5 days")
        self.assertEqual(h.describe("VM_CRASHED", "", "Plex"), ("Plex crashed", ""))
        self.assertEqual(h.describe("SVM_HOST_BACK", "tico answers again", "tico")[0], "tico answers again")
        self.assertEqual(h.bracket("fe80::1"), "[fe80::1]")
        self.assertEqual(h.bracket("10.0.0.1:8443"), "10.0.0.1:8443")
        conf = h.merged(h.DEFAULT_CONFIG, {"claude": {"review": {"day": "Friday"}}, "new": 1})
        self.assertEqual((conf["claude"]["review"]["day"], conf["claude"]["review"]["at"], conf["new"]), ("Friday", "08:00", 1))

    def test_setup_remembers_a_no(self):
        """A yes or no asked again at the next setup offers the last answer (Enter keeps it)."""
        offered = []
        real = h.ui.yes
        h.ui.yes = lambda prompt, default: offered.append(default) or default
        try:
            conf = {}
            self.assertTrue(h.asked(conf, "claude_code", "Install Claude Code?"))
            conf["answers"]["claude_code"] = False  # (said no that time)
            self.assertFalse(h.asked(conf, "claude_code", "Install Claude Code?"))
            self.assertEqual(offered, [True, False])
        finally:
            h.ui.yes = real

    def test_storage_remount_and_made_up_run(self):
        fp = FakePool()
        now = time.time()
        state = {"attached": {"u-p3": now - 7200}}  # attached before godofredo's start, not after
        lk = look_at(fp, state=state, now=now)
        done = h.storage(lk) + h.remount(lk)
        self.assertEqual(fp.plugged, ["P3"])
        self.assertIn("Local storage on tico is 95% full", done[0])
        self.assertEqual(h.storage(lk), [])  # once a day
        # a daily run that should have run 2 hours ago (pool clock 3 h behind UTC)
        due_local = time.gmtime(now - 2 * 3600 - 3 * 3600)
        fp.vmss["V1"] = {"uuid": "u-v1", "name_label": "Nightly", "enabled": True, "VMs": ["X"], "frequency": "daily", "schedule": {"hour": str(due_local.tm_hour), "min": str(due_local.tm_min)}, "last_run_time": xmlrpc.client.DateTime(xd(now - 26 * 3600))}
        done = h.schedules(lk)
        self.assertEqual(fp.plugin_calls, [("vmss", "snapshot_now", {"vmss_uuid": "u-v1"})])
        self.assertIn("Made up the missed %02d:%02d run of 'Nightly'" % (due_local.tm_hour, due_local.tm_min), done[0])
        self.assertEqual(h.schedules(lk), [])  # once

    def test_forward_services_and_heartbeat(self):
        srv, port = serve(Hook)
        try:
            fp = FakePool()
            fp.other_config[h.SETTINGS_KEY] = json.dumps({"channels": {"webhook": {"on": True, "url": "http://127.0.0.1:%d/hook" % port}}, "min_priority": 3})
            state = {}
            lk = look_at(fp, {"watch": {"services": [{"name": "Home Assistant", "url": "http://127.0.0.1:%d/down" % port}, {"name": "NAS", "url": "http://127.0.0.1:%d/up" % port}]}}, state)
            h.forward(lk)  # the first look only starts the clock
            time.sleep(1.1)
            fp.x.message.create("VM_CRASHED", "2", "VM", "u-x", "")
            for _ in range(3):
                h.services(lk)
            self.assertTrue(state["services"]["http://127.0.0.1:%d/down" % port].get("down_since"))
            lk.now = time.time()
            done = h.forward(lk)
            self.assertEqual(done, ["sent 2 alerts"])
            titles = [m["title"] for m in Hook.got]
            self.assertIn("A VM crashed", titles)  # (its VM isn't in the pretend pool: no name)
            self.assertTrue(any(t.startswith("Home Assistant isn't answering") for t in titles))
            h.heartbeat(lk, time.time() - 60, force=True)
            beat = json.loads(fp.other_config[h.STATUS_KEY])
            self.assertEqual((beat["version"], beat["claude"]), (h.VERSION, False))
        finally:
            srv.shutdown()

    def test_a_whole_minute(self):
        """The service's look, end to end on the pretend pool: the steps, its heartbeat, and the
        jobs left alone when asked (as 'svm-helper check' does)."""
        fp = FakePool()
        conn = FakeConn(fp.x)
        conn.close = lambda: None
        helper = h.Helper(jobs=False)
        helper.conf = h.merged(h.DEFAULT_CONFIG, {"claude": {"advice": {"at": time.strftime("%H:%M", time.localtime(time.time() - 60))}}, "watch": {"clocks": False}})
        helper.pool = conn
        done = helper.tick()
        self.assertTrue(any("Local storage on tico is 95% full" in d for d in done), done)
        st = h.load_json(h.state_path("state"), {})
        self.assertEqual((st["pool_name"], st["hosts_seen"]["127.0.0.2"]), ("home", "godofredo"))
        self.assertIn(h.STATUS_KEY, fp.other_config)
        self.assertNotIn("checking the pool for the Advisor", done)  # (jobs left alone)
        self.assertEqual(helper.jobs, {})

    @unittest.skipUnless(HAVE_ANTHROPIC, "needs Anthropic's SDK")
    def test_claude_answers(self):
        """Claude's answer read from its text blocks; a refusal is no answer; an SDK without
        'fallbacks' gets the plain request (no request leaves this computer)."""
        from types import SimpleNamespace as SN
        asked = []

        def reply(stop, *texts):
            return SN(stop_reason=stop, content=[SN(type="thinking", thinking="")] + [SN(type="text", text=t) for t in texts])

        class Beta:
            def __init__(self, answer, old_sdk=False):
                self.answer, self.old = answer, old_sdk

            def create(self, **kw):
                asked.append(kw)
                if self.old:
                    raise TypeError("unexpected keyword argument 'fallbacks'")
                return self.answer

        c = h.Claude({"language": "pt-BR"}, {"anthropic_api_key": "k"})
        for answer, old_sdk, want in ((reply("end_turn", "O servidor ", "caiu."), False, "O servidor caiu."), (reply("refusal"), False, None), (reply("end_turn", "ok"), True, "ok")):
            api = Beta(answer, old_sdk)
            plain = Beta(answer)
            c.client = lambda api=api, plain=plain: SN(beta=SN(messages=api), messages=plain)
            self.assertEqual(c.explain("tico isn't answering", "", "SVM_HOST_UNREACHABLE", {"pool": "home"}), want)
        self.assertEqual((asked[0]["model"], asked[0]["fallbacks"], asked[0]["output_config"]), ("claude-opus-5-5", "default", {"effort": "low"}))
        self.assertIn("Brazilian Portuguese", asked[0]["system"])
        self.assertNotIn("fallbacks", asked[-1])

    def test_claude_through_the_subscription(self):
        """On the subscription, Claude Code answers in print mode: no tools, no session kept, the
        token in its environment and no API key, the prompt on stdin; its errors are no answer."""
        folder = tempfile.mkdtemp(dir=TMP)
        seen = os.path.join(folder, "seen.json")
        fake = os.path.join(folder, "claude")
        with open(fake, "w") as f:
            f.write("#!%s\nimport json, os, sys\nprompt = sys.stdin.read()\njson.dump({'args': sys.argv[1:], 'token': os.environ.get('CLAUDE_CODE_OAUTH_TOKEN'), 'key': os.environ.get('ANTHROPIC_API_KEY'), 'prompt': prompt}, open(%r, 'w'))\n"
                    "print(json.dumps({'type': 'result', 'is_error': 'fail' in prompt, 'result': 'usage limit reached' if 'fail' in prompt else 'Tudo certo.'}))\n" % (sys.executable, seen))
        os.chmod(fake, 0o755)
        real = h.claude_bin
        h.claude_bin = lambda: fake
        os.environ["ANTHROPIC_API_KEY"] = "sk-ant-should-not-be-used"
        try:
            c = h.Claude({"claude": {"auth": "subscription"}, "language": "pt-BR"}, {"claude_oauth_token": "sk-ant-oat01-x"})
            self.assertEqual((c.ready(), c.how()), (True, "your Claude subscription"))
            self.assertEqual(c.explain("tico isn't answering", "", "SVM_HOST_UNREACHABLE", {"pool": "home"}), "Tudo certo.")
            got = json.load(open(seen))
            self.assertEqual((got["token"], got["key"]), ("sk-ant-oat01-x", None))
            a = got["args"]
            self.assertEqual(a[:3], ["-p", "--output-format", "json"])
            self.assertEqual(a[a.index("--tools") + 1], "")
            self.assertIn("--no-session-persistence", a)
            self.assertEqual(a[a.index("--effort") + 1], "low")
            self.assertIn("Brazilian Portuguese", a[a.index("--system-prompt") + 1])
            self.assertIn("tico isn't answering", got["prompt"])
            self.assertIsNone(c.write("system", "please fail", "low"))
            self.assertIn("usage limit reached", c.last_error)
            # the key is what decides when no way was chosen
            self.assertEqual(h.Claude({}, {"anthropic_api_key": "sk-ant-k"}).mode, "api")
            self.assertFalse(h.Claude({"claude": {"auth": "subscription"}}, {}).ready())
        finally:
            h.claude_bin = real
            del os.environ["ANTHROPIC_API_KEY"]

    def test_plain_review(self):
        fp = FakePool()
        x = fp.x
        x.VM = NS()
        x.VM.get_all_records = lambda: {"A": {"uuid": "u-a", "name_label": "Plex", "is_a_template": False, "is_a_snapshot": False, "is_control_domain": False, "power_state": "Running", "resident_on": "H1", "other_config": {}, "memory_dynamic_max": str(2 * 1024 ** 3), "VCPUs_max": "2"}}
        x.VDI = NS()
        x.VDI.get_all_records = lambda: {"D": {"type": "user", "is_a_snapshot": False, "VBDs": [], "managed": True, "SR": "S2", "name_label": "old disk", "virtual_size": str(20 * 1024 ** 3)}}
        snap = h.snapshot_of_pool(x, {"clock": {"u-h2": 250}})
        text = h.plain_review(snap)
        self.assertIn("Local storage is 95% full", text)
        self.assertIn("godofredo's clock is 4 min off", text)
        self.assertIn("1 disks aren't used by any VM", text)
        self.assertIn("VMs: 1, 1 running", text)


def advisor_pool():
    """A pretend pool with something to say for most of the Advisor's checks."""
    fp = FakePool()
    x = fp.x
    now = time.time()
    fp.vms = {
        "A": {"uuid": "u-a", "name_label": "Plex", "is_a_template": False, "is_a_snapshot": False, "is_control_domain": False, "power_state": "Running", "resident_on": "H1", "other_config": {}, "guest_metrics": "G1", "snapshot_schedule": "OpaqueRef:NULL", "snapshots": ["SA"], "memory_dynamic_max": str(2 * 1024 ** 3), "VCPUs_max": "8", "VBDs": ["VB1", "VB2"]},
        "B": {"uuid": "u-b", "name_label": "dc01", "is_a_template": False, "is_a_snapshot": False, "is_control_domain": False, "power_state": "Running", "resident_on": "H1", "other_config": {"auto_poweron": "true"}, "guest_metrics": "OpaqueRef:NULL", "snapshot_schedule": "V1", "snapshots": [], "memory_dynamic_max": str(4 * 1024 ** 3), "VCPUs_max": "2", "VBDs": ["VB3"]},
        "O": {"uuid": "u-o", "name_label": "old-test", "is_a_template": False, "is_a_snapshot": False, "is_control_domain": False, "power_state": "Halted", "resident_on": "OpaqueRef:NULL", "other_config": {}, "guest_metrics": "G2", "metrics": "M-O", "snapshot_schedule": "V1", "snapshots": [], "memory_dynamic_max": str(1024 ** 3), "VCPUs_max": "1", "VBDs": []},
        "H": {"uuid": "u-helper", "name_label": "svm-helper", "is_a_template": False, "is_a_snapshot": False, "is_control_domain": False, "power_state": "Running", "resident_on": "H1", "other_config": {}, "guest_metrics": "G1", "snapshot_schedule": "OpaqueRef:NULL", "snapshots": [], "memory_dynamic_max": str(1024 ** 3), "VCPUs_max": "1"},
        "SA": {"uuid": "u-sa", "name_label": "before the update", "is_a_template": False, "is_a_snapshot": True, "is_control_domain": False, "snapshot_of": "A", "snapshot_time": xmlrpc.client.DateTime(xd(now - 40 * 86400)), "is_vmss_snapshot": False, "power_state": "Halted", "other_config": {}},
    }
    x.VM = NS()
    x.VM.get_all_records = lambda: fp.vms
    x.VM_guest_metrics = NS()
    x.VM_guest_metrics.get_all_records = lambda: {"G1": {"PV_drivers_detected": True, "os_version": {"name": "Ubuntu 24.04.1 LTS"}}, "G2": {"PV_drivers_detected": True, "os_version": {"name": "CentOS Linux release 7.9.2009 (Core)"}}}
    x.VM_metrics = NS()
    x.VM_metrics.get_all_records = lambda: {"M-O": {"start_time": xmlrpc.client.DateTime(xd(now - 200 * 86400))}}
    x.VDI = NS()
    x.VDI.get_all_records = lambda: {
        "D": {"uuid": "u-d", "type": "user", "is_a_snapshot": False, "VBDs": [], "managed": True, "SR": "S2", "name_label": "old disk", "virtual_size": str(20 * 1024 ** 3)},
        "DA": {"uuid": "u-da", "type": "user", "is_a_snapshot": False, "VBDs": ["VB1"], "managed": True, "SR": "S1", "name_label": "Plex disk", "virtual_size": str(40 * 1024 ** 3)},
        "DB": {"uuid": "u-db", "type": "user", "is_a_snapshot": False, "VBDs": ["VB3"], "managed": True, "SR": "S2", "name_label": "dc01 disk", "virtual_size": str(40 * 1024 ** 3)},
        "ISO": {"uuid": "u-iso", "type": "user", "is_a_snapshot": False, "VBDs": ["VB2"], "managed": True, "SR": "S-iso", "name_label": "ubuntu.iso", "virtual_size": "1"},
    }
    x.VBD = NS()
    x.VBD.get_all_records = lambda: {"VB1": {"type": "Disk", "VDI": "DA", "empty": False}, "VB2": {"type": "CD", "VDI": "ISO", "empty": False}, "VB3": {"type": "Disk", "VDI": "DB", "empty": False}}
    fp.srs["S-iso"] = {"uuid": "u-siso", "name_label": "ISOs", "type": "iso", "content_type": "iso", "shared": True, "physical_size": "0", "physical_utilisation": "0", "other_config": {}}
    x.Certificate = NS()
    x.Certificate.get_all_records = lambda: {"C1": {"host": "H1", "not_after": xmlrpc.client.DateTime(xd(now + 10 * 86400)), "name": ""}, "C2": {"host": "H2", "not_after": xmlrpc.client.DateTime(xd(now + 900 * 86400)), "name": ""}}
    fp.hosts["H2"]["latest_synced_updates_applied"] = "no"
    fp.hosts["H1"]["latest_synced_updates_applied"] = "yes"
    return fp


class Advisor(unittest.TestCase):
    """The Advisor: good practice checked, worded by Claude, and what the app asks for."""

    def test_the_checks(self):
        fp = advisor_pool()
        load = {"host": {"u-h1": {"cpu": (85, 99)}, "u-h2": {"cpu": (3, 10)}}, "vm": {"u-a": {"cpu": (4, 9), "memory": (95, 99)}, "u-b": {"cpu": (20, 50)}}}
        checks = h.advisor_checks(fp.x, h.merged(h.DEFAULT_CONFIG, {"self_vm": "u-helper"}), {"clock": {"u-h1": 1, "u-h2": 250}}, load=load)
        by = {c["id"]: c for c in checks}
        # the new ones, and what the app can fix
        self.assertEqual((by["iso_in_drive"]["objects"], by["iso_in_drive"]["fix"]), (["Plex (ubuntu.iso)"], {"action": "eject", "uuids": ["u-a"], "names": ["Plex (ubuntu.iso)"]}))
        self.assertEqual(by["local_storage"]["objects"], ["Plex"])  # its disk is on Local storage
        self.assertEqual(by["os_support"]["objects"], ["old-test (CentOS Linux release 7.9.2009 (Core))"])
        self.assertEqual(by["vms_off_long"]["objects"], ["old-test (started 200 days ago)"])
        self.assertEqual(by["vm_busy"]["objects"], ["Plex (memory 95%)"])
        self.assertEqual(by["vm_idle_cpus"]["objects"], ["Plex (8 vCPUs, at most 9% busy)"])
        self.assertEqual(by["server_load"]["objects"], ["tico (85% on average)"])
        self.assertEqual(by["one_server"]["objects"], ["tico"])
        self.assertEqual(by["default_storage"]["fix"]["action"], "vm_storage")
        self.assertEqual((by["autostart"]["fix"]["action"], by["autostart"]["fix"]["uuids"]), ("autostart", ["u-a"]))
        self.assertEqual(by["old_snapshots"]["fix"], {"action": "delete_snapshots", "uuids": ["u-sa"], "names": ["Plex: before the update (40 days old)"]})
        self.assertEqual(by["unused_disks"]["fix"]["uuids"], ["u-d"])
        self.assertEqual(by["snapshot_schedules"]["fix"]["action"], "schedule")
        self.assertEqual(by["certificates"]["fix"], {"action": "renew_cert", "uuids": ["u-h1"], "names": ["tico"]})
        self.assertNotIn("fix", by["vm_tools"])  # (nothing to do from the app)
        self.assertTrue(all("fix" not in c for c in checks if c["status"] == "ok"))
        self.assertTrue(all(c["topic"] in h.TOPICS and c["status"] in ("ok", "warn", "bad") for c in checks))
        self.assertEqual((by["snapshot_schedules"]["status"], by["snapshot_schedules"]["objects"]), ("warn", ["Plex"]))  # not the helper's own VM
        self.assertEqual((by["storage_space"]["status"], by["storage_space"]["objects"]), ("bad", ["Local storage (95%)"]))
        self.assertEqual(by["unused_disks"]["objects"], ["old disk (20 GB, on TrueNAS)"])
        self.assertEqual(by["old_snapshots"]["title"], "1 snapshot over a month old")
        self.assertEqual((by["clocks"]["status"], by["clocks"]["objects"]), ("bad", ["godofredo (4 min)"]))
        self.assertEqual(by["updates"]["objects"], ["godofredo"])
        self.assertEqual(by["same_version"]["status"], "warn")
        self.assertEqual(by["vm_tools"]["objects"], ["dc01"])
        self.assertEqual(by["autostart"]["objects"], ["Plex"])  # dc01 starts by itself; the helper's VM isn't counted
        self.assertEqual(by["autostart"]["title"], "1 running VM doesn't start by itself")
        self.assertEqual((by["certificates"]["status"], by["certificates"]["objects"]), ("bad", ["tico"]))
        self.assertEqual(by["alerts_sent"]["status"], "warn")
        self.assertEqual(by["servers_answering"]["status"], "ok")
        self.assertEqual(by["servers_answering"]["note"], "")  # what's fine needs no note
        # a VM the helper starts in order doesn't need to start by itself
        fp.other_config[h.SETTINGS_KEY] = json.dumps({"start_order": {"on": True, "vms": [{"uuid": "u-a", "wait": 30}]}, "channels": {"ntfy": {"on": True, "url": "https://ntfy.sh/x"}}})
        by = {c["id"]: c for c in h.advisor_checks(fp.x, h.merged(h.DEFAULT_CONFIG, {"self_vm": "u-helper"}), {})}
        self.assertEqual(by["autostart"]["status"], "ok")
        self.assertEqual(by["alerts_sent"]["status"], "ok")
        self.assertNotIn("clocks", by)  # not compared yet: nothing said

    def test_the_weeks_load(self):
        xml = (b"<xport><meta><rows>2</rows><legend><entry>AVERAGE:host:u-h1:cpu_avg</entry><entry>AVERAGE:host:u-h1:memory_total_kib</entry>"
               b"<entry>AVERAGE:host:u-h1:memory_free_kib</entry><entry>AVERAGE:vm:u-a:cpu0</entry><entry>AVERAGE:vm:u-a:cpu1</entry>"
               b"<entry>AVERAGE:vm:u-a:memory</entry><entry>AVERAGE:vm:u-a:memory_internal_free</entry></legend></meta><data>"
               b"<row><t>3600</t><v>0.5</v><v>1000</v><v>500</v><v>0.1</v><v>0.3</v><v>2048</v><v>1</v></row>"
               b"<row><t>7200</t><v>0.7</v><v>1000</v><v>250</v><v>NaN</v><v>0.5</v><v>2048</v><v>1</v></row></data></xport>")
        out = h.parse_rrd(xml, {"host": {}, "vm": {}})
        self.assertEqual(out["host"]["u-h1"], {"cpu": (60, 70), "memory": (62, 75)})
        self.assertEqual(out["vm"]["u-a"], {"cpu": (20, 20), "memory": (50, 50)})  # (an hour it didn't say is left out)

    def test_the_snapshot_says_more(self):
        fp = advisor_pool()
        snap = h.snapshot_of_pool(fp.x, {}, {"host": {"u-h1": {"cpu": (30, 60)}}, "vm": {"u-a": {"cpu": (4, 9), "memory": (95, 99)}}})
        plex = [v for v in snap["vms"] if v["name"] == "Plex"][0]
        self.assertEqual((plex["iso_in_drive"], plex["disks"][0]["storage"], plex["disks"][0]["shared_storage"], plex["system"]), ("ubuntu.iso", "Local storage", False, "Ubuntu 24.04.1 LTS"))
        self.assertEqual(plex["this_week"]["memory"], {"average_pct": 95, "peak_pct": 99})
        tico = [x for x in snap["servers"] if x["name"] == "tico"][0]
        self.assertEqual(tico["this_week"]["processors"], {"average_pct": 30, "peak_pct": 60})
        old = [v for v in snap["vms"] if v["name"] == "old-test"][0]
        self.assertEqual(old["last_started_days_ago"], 200)
        json.dumps(snap)  # (it goes to Claude as JSON)

    def test_an_update_asked_by_the_app(self):
        fp = advisor_pool()
        real = h.UPDATER
        h.UPDATER = os.path.join(TMP, "update.sh")
        with open(h.UPDATER, "w") as f:
            f.write("#!/bin/sh\n")
        os.chmod(h.UPDATER, 0o755)
        request = h.state_path("update").replace(".json", ".request")
        try:
            self.assertFalse(h.ask_update("v1.30.0; rm -rf /"))
            self.assertFalse(os.path.exists(request))
            fp.other_config[h.REQUEST_KEY] = json.dumps({"update": 77, "update_ref": "v1.30.0"})
            record = fp.x.pool.get_record
            fp.x.pool.get_record = lambda p: dict(record(p), other_config=fp.other_config)
            helper = h.Helper(jobs=True)
            helper.spawn = lambda name, fn: True
            helper.pool = FakeConn(fp.x)
            state = {}
            done = helper.look(state, time.time())
            self.assertIn("updating to v1.30.0, as the app asked", done)
            self.assertEqual(open(request).read(), "v1.30.0\n")
            self.assertEqual(json.loads(fp.other_config[h.STATUS_KEY])["updates"], 1)
            os.remove(request)
            helper.look(state, time.time())
            self.assertFalse(os.path.exists(request))  # answered once
        finally:
            h.UPDATER = real
            if os.path.exists(request):
                os.remove(request)

    def test_claude_words_them(self):
        checks = [{"id": "clocks", "topic": "servers", "status": "bad", "title": "Clock off: godofredo (4 min)", "note": "English note", "objects": ["godofredo (4 min)"]}, {"id": "memory", "topic": "servers", "status": "ok", "title": "Every server has memory to spare", "note": "", "objects": []}]
        answer = 'Aqui está:\n```json\n{"checks": {"clocks": {"title": "Relógio errado: godofredo (4 min)", "note": "Ligue o NTP."}, "memory": {"title": "Todos têm memória"}, "nope": {"title": "x"}}, "more": [{"topic": "vms", "status": "warn", "title": "dc01 tem 1 vCPU só", "note": "..."}, {"topic": "elsewhere", "status": "warn", "title": "ignored"}]}\n```'
        out = h.worded(checks, h.json_answer(answer))
        self.assertEqual([c["title"] for c in out], ["Relógio errado: godofredo (4 min)", "Todos têm memória", "dc01 tem 1 vCPU só"])
        self.assertEqual(out[0]["note"], "Ligue o NTP.")
        self.assertEqual(out[0]["status"], "bad")  # its status stays the helper's
        self.assertEqual((out[2]["by"], out[2]["topic"], out[2]["id"]), ("claude", "vms", "claude_1"))
        self.assertIsNone(h.json_answer("no JSON here"))

    def setUp(self):
        # (its looks save the helper's state: the other tests' is put back after)
        self.kept = {n: open(h.state_path(n)).read() for n in ("state", "advice", "test") if os.path.exists(h.state_path(n))}

    def tearDown(self):
        for n in ("state", "advice", "test"):
            if n in self.kept:
                with open(h.state_path(n), "w") as f:
                    f.write(self.kept[n])
            elif os.path.exists(h.state_path(n)):
                os.remove(h.state_path(n))

    def test_written_for_the_app_and_asked_by_it(self):
        fp = advisor_pool()
        conf = h.merged(h.DEFAULT_CONFIG, {"claude": {"auth": "off"}})
        advice = h.write_advice(FakeConn(fp.x), conf, {}, request=1234)
        saved = json.loads(fp.other_config[h.ADVICE_KEY])
        self.assertEqual((saved["by"], saved["request"], saved["language"]), ("plain", 1234, "en"))
        self.assertEqual(saved["checks"], advice["checks"])
        self.assertEqual(h.load_json(h.state_path("advice"), {})["bad"], sum(c["status"] == "bad" for c in advice["checks"]))
        # the app asks: the next look starts it once, and the heartbeat says it answers requests
        fp.other_config[h.REQUEST_KEY] = json.dumps({"advice": 5678, "test": 99})
        record = fp.x.pool.get_record
        fp.x.pool.get_record = lambda p: dict(record(p), other_config=fp.other_config)
        self.assertEqual(h.requests_of(fp.x.pool.get_record("POOL")), {"advice": 5678.0, "test": 99.0})
        helper = h.Helper(jobs=True)
        helper.conf = conf
        started = []
        helper.spawn = lambda name, fn: started.append(name) or True
        helper.pool = FakeConn(fp.x)
        state = {}
        helper.look(state, time.time())
        self.assertIn("advisor", started)
        self.assertIn("test alert", started)
        self.assertEqual(state["answered"], {"advice": 5678.0, "test": 99.0})
        started.clear()
        helper.look(state, time.time())
        self.assertNotIn("test alert", started)  # answered once
        beat = json.loads(fp.other_config[h.STATUS_KEY])
        self.assertEqual((beat["requests"], beat["advice"]["request"]), (1, 1234))
        # a test alert, as asked: what each channel said, in the heartbeat
        h.send_test(FakeConn(fp.x), {}, "home", 99)
        beat = json.loads(fp.other_config[h.STATUS_KEY])
        self.assertEqual((beat["test"]["request"], beat["test"]["results"]), (99, {}))


# ---------------------------------------------------------------- a pretend XAPI over HTTPS

def make_cert(folder):
    key, crt = os.path.join(folder, "k.pem"), os.path.join(folder, "c.pem")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key, "-out", crt, "-days", "2", "-subj", "/CN=xapi-test"], check=True, capture_output=True)
    der = ssl.PEM_cert_to_DER_cert(open(crt).read())
    return key, crt, hashlib.sha256(der).hexdigest()


class FakeXapi:
    """Answers XAPI's XML-RPC (as a coordinator, or a member that sends you to it) and GET /export."""

    def __init__(self, coordinator=None, export_ok=True):
        self.imported = {}
        self.coordinator = coordinator
        self.address = ""
        self.snapshots = 0
        self.messages = []
        self.export_ok = export_ok
        self.tasks = {}

    def call(self, method, params):
        ok = lambda v: {"Status": "Success", "Value": v}
        if method == "session.login_with_password":
            if self.coordinator:
                return {"Status": "Failure", "ErrorDescription": ["HOST_IS_SLAVE", self.coordinator]}
            if params[:2] != ("root", "secret"):
                return {"Status": "Failure", "ErrorDescription": ["SESSION_AUTHENTICATION_FAILED", params[0], "Authentication failure"]}
            return ok("OpaqueRef:session")
        if method == "session.logout":
            return ok("")
        if method == "pool.get_all":
            return ok(["OpaqueRef:pool"])
        if method == "pool.get_name_label":
            return ok("home")
        if method == "task.create":
            t = "OpaqueRef:task%d" % len(self.tasks)
            self.tasks[t] = "pending"
            return ok(t)
        if method == "task.get_status":
            return ok(self.tasks.get(params[1], "failure"))
        if method == "task.get_error_info":
            return ok([] if self.tasks.get(params[1]) == "success" else ["VDI_IO_ERROR", "Device I/O errors"])
        if method in ("task.destroy", "task.cancel"):
            return ok("")
        if method == "VDI.get_by_uuid":
            return ok("OpaqueRef:vdi1")
        if method == "pool.get_master":
            return ok("OpaqueRef:h1")
        if method == "host.get_all_records":
            return ok({"OpaqueRef:h1": {"uuid": "u-h1", "name_label": "tico", "address": self.address, "software_version": {"product_brand": "XenServer", "product_version": "8.4.0"}}})
        if method == "VM.get_all_records":
            return ok({"OpaqueRef:v1": {"uuid": "u-v1", "name_label": "Home Assistant", "is_a_template": False, "is_a_snapshot": False, "is_control_domain": False, "resident_on": "OpaqueRef:h1", "other_config": {}}})
        if method == "pool.get_uuid":
            return ok("u-pool")
        if method == "VM.get_by_uuid":
            return ok("OpaqueRef:v1") if params[1] == "u-v1" else {"Status": "Failure", "ErrorDescription": ["UUID_INVALID", "VM", params[1]]}
        if method == "VM.get_name_label":
            return ok("Home Assistant")
        if method == "VM.get_uuid":
            return ok("u-snap" if params[1] == "OpaqueRef:snap" else "u-v1")
        if method == "VM.snapshot":
            self.snapshots += 1
            return ok("OpaqueRef:snap")
        if method == "VM.destroy":
            self.snapshots -= 1
            return ok("")
        if method == "VM.get_VBDs":
            return ok([])
        if method == "VM.get_suspend_VDI":
            return ok("OpaqueRef:NULL")
        if method == "message.create":
            self.messages.append(params[1:])
            return ok("OpaqueRef:msg")
        if method == "host.call_plugin":
            return {"Status": "Failure", "ErrorDescription": ["XENAPI_MISSING_PLUGIN", params[2]]}
        return {"Status": "Failure", "ErrorDescription": ["MESSAGE_METHOD_UNKNOWN", method]}

    def handler(self):
        api = self

        class H(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                params, method = xmlrpc.client.loads(body)
                out = xmlrpc.client.dumps((api.call(method, params),), methodresponse=True, allow_none=True).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/xml")
                self.send_header("Content-Length", str(len(out)))
                self.end_headers()
                self.wfile.write(out)

            def do_GET(self):
                q = dict(p.split("=", 1) for p in self.path.split("?", 1)[1].split("&"))
                assert q["session_id"] == "OpaqueRef%3Asession"
                api.tasks["OpaqueRef:" + q["task_id"].split("%3A", 1)[1]] = "success" if api.export_ok else "failure"
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"XVA" * 100000)

            def do_PUT(self):
                q = dict(p.split("=", 1) for p in self.path.split("?", 1)[1].split("&"))
                assert q["session_id"] == "OpaqueRef%3Asession"
                body = self.rfile.read(int(self.headers["Content-Length"]))
                api.imported[urllib.parse.unquote(q["vdi"])] = body
                api.tasks[urllib.parse.unquote(q["task_id"])] = "success"
                self.send_response(200)
                self.end_headers()

            def log_message(self, *a):
                pass

        return H


@unittest.skipUnless(HAVE_XENAPI and shutil.which("openssl"), "needs the XenAPI package and openssl")
class Connection(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.key, cls.crt, cls.fp = make_cert(TMP)
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(cls.crt, cls.key)
        cls.coord_api = FakeXapi()
        cls.coord, cport = serve(cls.coord_api.handler(), ctx)
        cls.member, mport = serve(FakeXapi(coordinator="127.0.0.1:%d" % cport).handler(), ctx)
        cls.caddr, cls.maddr = "127.0.0.1:%d" % cport, "127.0.0.1:%d" % mport
        cls.coord_api.address = cls.caddr

    @classmethod
    def tearDownClass(cls):
        cls.coord.shutdown()
        cls.member.shutdown()

    def conf(self, *hosts):
        return {"pool": {"hosts": list(hosts), "user": "root", "pins": [self.fp]}}

    def test_member_sends_it_to_the_coordinator(self):
        p = h.Pool(self.conf(self.maddr), {"pool_password": "secret"}).connect()
        self.assertEqual((p.address, p.sid), (self.caddr, "OpaqueRef:session"))
        self.assertEqual(p.x.pool.get_name_label("OpaqueRef:pool"), "home")
        p.close()

    def test_unknown_certificate_is_refused(self):
        with self.assertRaises(h.CertificateChanged):
            h.Pool({"pool": {"hosts": [self.caddr], "pins": ["00" * 32]}}, {"pool_password": "secret"}).connect()

    def test_wrong_password_and_nobody_answering(self):
        with self.assertRaises(h.PoolError) as e:
            h.Pool(self.conf(self.caddr), {"pool_password": "nope"}).connect()
        self.assertIn("refused", str(e.exception))
        with self.assertRaises(h.PoolError) as e:
            h.Pool(self.conf("127.0.0.1:1"), {"pool_password": "secret"}).connect()
        self.assertIn("no server of the pool answered", str(e.exception))

    def test_setup_meets_the_pool(self):
        """The setup's first questions, answered as a person would: a member's address, the
        password, yes to its certificate; it's sent on to the coordinator, whose certificate (the
        same here) is already trusted."""
        import builtins
        import getpass as gp
        import io
        from contextlib import redirect_stdout
        answers = iter([self.maddr, "root", "y"])
        real_input, real_getpass = builtins.input, gp.getpass
        builtins.input = lambda prompt="": next(answers)
        gp.getpass = lambda prompt="": "secret"
        out = io.StringIO()
        conf, sec = h.load_config(), {}
        try:
            with redirect_stdout(out):
                s, x, pool_ref, name, hosts, vms = h.setup_pool(1, 5, conf, sec)
            s.xenapi.session.logout()
        finally:
            builtins.input, gp.getpass = real_input, real_getpass
        text = out.getvalue()
        self.assertIn("is a member: its coordinator is " + self.caddr, text)
        self.assertIn("Pool home: 1 server, 1 VMs, XenServer 8.4.0", text)
        self.assertEqual(conf["pool"], {"hosts": [self.caddr], "user": "root", "pins": [self.fp]})
        self.assertEqual(sec["pool_password"], "secret")
        self.assertEqual(name, "home")

    def test_a_disk_copy_goes_into_its_vdi(self):
        srv, port = serve(Source)
        try:
            p = h.Pool(self.conf(self.caddr), {"pool_password": "secret"}).connect()
            task = p.x.task.create("Write a converted disk", "dc01 disk")
            job = {"kind": "http", "url": "http://127.0.0.1:%d/folder/x-flat.vmdk" % port, "user": "root", "size": len(Source.data), "vdi": "u-vdi1", "format": "raw"}
            self.assertEqual(h.copy_disk(p, task, job, "secret"), len(Source.data))
            self.assertEqual(self.coord_api.imported["OpaqueRef:vdi1"], Source.data)
            self.assertEqual(p.x.task.get_status(task), "success")
            p.close()
        finally:
            srv.shutdown()


HAVE_SEGNO = importlib.util.find_spec("segno") is not None

# A pretend 'claude setup-token': its screen as Claude Code draws it (words placed by cursor
# jumps, the link also as a terminal hyperlink), then the code read in raw mode, as Ink does.
FAKE_SETUP_TOKEN = r'''
import os, sys, tty
link = "https://claude.ai/oauth/authorize?FAKE%d" % os.getpid()
out = os.fdopen(1, "w", encoding="utf-8")
out.write("\x1b[2GBrowser\x1b[10Gdidn't\x1b[17Gopen?\r\r\n\r\r\n\x1b]8;id=x;%s\x07\x1b[38;5;246m%s\x1b[39m\x1b]8;;\x07\r\r\n\r\r\n\x1b[2GPaste\x1b[8Gcode\r\r\n\x1b[2G\x1b[38;5;153m╭──╮\x1b[39m\r\r\n" % (link, link))
out.flush()
tty.setraw(0)
given = b""
while True:
    c = os.read(0, 1)
    if not c:
        sys.exit(1)
    if c in b"\r\n":
        break
    given += c
if given == b"good#state":
    out.write("\x1b[32m✓ Long-lived authentication token created successfully!\x1b[39m\r\r\n\r\r\nYour OAuth token (valid for 1 year):\r\r\n\r\r\n\x1b[33m" + sys.argv[1] + "\x1b[39m\r\r\n\r\r\nStore this token securely.\r\r\n")
    out.flush()
    sys.exit(0)
out.write("\x1b[31m✘ OAuth error: That code wasn't accepted.\x1b[39m\r\r\n\r\r\n Press Enter to retry\r\r\n")
out.flush()
os.read(0, 100)
'''


class PhoneSignIn(unittest.TestCase):
    """Signing in to Claude from a phone: the QR code, Claude Code's screen read, and the page."""

    TOKEN = "sk-ant-oat01-" + "Ab1_-cD2" * 12

    @unittest.skipUnless(HAVE_SEGNO, "needs segno")
    def test_the_qr_code_is_the_text(self):
        import segno
        url = "http://192.168.10.57:43127/abcdefgh"
        lines = h.qr_lines(url)
        light = {"█": (True, True), "▀": (True, False), "▄": (False, True), " ": (False, False)}
        drawn = []
        for line in lines:
            drawn.append([light[c][0] for c in line])
            drawn.append([light[c][1] for c in line])
        want = [[not m for m in row] for row in segno.make_qr(url, error="m").matrix_iter(scale=1, border=2)]
        self.assertEqual(drawn[:len(want)], want)
        if len(drawn) > len(want):
            self.assertTrue(all(drawn[-1]))  # the row added to make them even is light
        self.assertLessEqual(len(lines[0]), 40)  # it fits an 80-column console, with room
        self.assertLessEqual(len(lines), 20)
        # on a Linux console: full blocks only, two to a dot; in capitals, a smaller code
        url = url.upper()
        wide = h.qr_lines(url, wide=True)
        self.assertEqual(set("".join(wide)), {"█", " "})
        want = [[not m for m in row] for row in segno.make_qr(url, error="m").matrix_iter(scale=1, border=2)]
        self.assertEqual([[line[i] == "█" for i in range(0, len(line), 2)] for line in wide], want)
        self.assertEqual(len(wide), 29)

    def test_what_the_screen_says(self):
        raw = "\x1b]8;id=x;https://claude.ai/oauth/authorize?Ab-1\x07\x1b[38;5;246mhttps://claude.ai/oauth/authorize?Ab-1\x1b[39m\x1b]8;;\x07\r\n"
        self.assertEqual(h.SIGN_IN_LINK.search(h.screen_text(raw)).group(0), "https://claude.ai/oauth/authorize?Ab-1")
        self.assertIsNone(h.SIGN_IN_LINK.search(h.screen_text(raw[:30])))  # half a link, still coming
        self.assertEqual(h.screen_text("\x1b[2GPaste\x1b[8Gcode"), " Paste code")
        self.assertIsNone(h.OAUTH_TOKEN.search(self.TOKEN[:-3]))
        self.assertEqual(h.OAUTH_TOKEN.search(h.screen_text("\x1b[33m" + self.TOKEN + "\x1b[39m\r\n")).group(0), self.TOKEN)
        self.assertEqual(h.page_language("pt-BR,pt;q=0.9,en;q=0.8"), "pt")
        self.assertEqual(h.page_language("de-DE,es;q=0.5"), "es")
        self.assertEqual(h.page_language(None), "en")

    def test_signed_in_through_the_page(self):
        script = os.path.join(TMP, "fake-setup-token.py")
        with open(script, "w") as f:
            f.write(FAKE_SETUP_TOKEN)
        sign_in = h.SignIn([sys.executable, "-I", script, self.TOKEN], cwd=TMP)
        link = sign_in.start(timeout=20)
        self.assertTrue(link.startswith("https://claude.ai/oauth/authorize?FAKE"))
        page = h.PhonePage(sign_in, link, address="127.0.0.1")
        url = "http://127.0.0.1:%d/%s" % (page.port, page.key)

        def fetch(path=url, given=None, lang="pt-BR,pt;q=0.9"):
            data = urllib.parse.urlencode({"code": given}).encode() if given is not None else None
            req = urllib.request.Request(path, data=data, headers={"Accept-Language": lang})
            try:
                with urllib.request.urlopen(req, timeout=30) as r:
                    return r.status, r.read().decode()
            except urllib.error.HTTPError as e:
                return e.code, ""

        try:
            self.assertEqual(fetch("http://127.0.0.1:%d/wrong" % page.port)[0], 404)
            self.assertEqual(fetch(url.upper().replace("HTTP://", "http://"))[0], 200)  # as the QR code has it
            status, body = fetch()
            self.assertEqual(status, 200)
            self.assertIn("Entrar no Claude", body)
            self.assertIn(link, body)
            self.assertIn("Cole o código primeiro", fetch(given="  ")[1])
            status, body = fetch(given="bad#state", lang="en")
            self.assertIn("accept that code", body)
            self.assertNotEqual(page.link, link)  # a new sign-in, and its link on the page
            self.assertIn(page.link, body)
            self.assertEqual(len(page.notes), 1)
            self.assertFalse(page.done.is_set())
            status, body = fetch(given="good#state")
            self.assertIn("Pronto", body)
            self.assertTrue(page.done.is_set())
            self.assertEqual(page.token, self.TOKEN)
            self.assertNotIn(self.TOKEN, body)  # it never goes to the phone
            self.assertIn("Pronto", fetch()[1])
            h.ui.color = False
            self.assertEqual(h.wait_for_phone(page, timeout=5), self.TOKEN)
        finally:
            page.close()
            sign_in.stop()


if __name__ == "__main__":
    try:
        unittest.main(verbosity=2)
    finally:
        shutil.rmtree(TMP, ignore_errors=True)
