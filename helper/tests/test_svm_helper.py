"""Tests of the Server Helper (helper/svm-helper) against a pretend pool: python3 helper/tests/test_svm_helper.py"""

import datetime
import http.server
import importlib.machinery
import importlib.util
import json
import os
import sys
import tempfile
import threading
import time
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.dont_write_bytecode = True
loader = importlib.machinery.SourceFileLoader("svm_helper", os.path.join(HERE, "..", "svm-helper"))
spec = importlib.util.spec_from_loader("svm_helper", loader)
h = importlib.util.module_from_spec(spec)
loader.exec_module(h)


def xd(epoch):
    return time.strftime("%Y%m%dT%H:%M:%SZ", time.gmtime(epoch))


class Pool:
    """A pretend pool: what XenAPI would answer, kept in dicts."""

    def __init__(self):
        now = time.time()
        self.messages = []
        self.plugged = []
        self.started = []
        self.plugin_calls = []
        self.other_config = {}
        self.hosts = {
            "H1": {"uuid": "u-h1", "name_label": "tico", "enabled": True, "other_config": {"boot_time": str(now - 3600)}},
            "H2": {"uuid": "u-h2", "name_label": "godofredo", "enabled": True, "other_config": {"boot_time": str(now - 600)}},
        }
        self.srs = {
            "S1": {"uuid": "u-s1", "name_label": "Local storage", "type": "ext", "content_type": "user", "shared": False, "physical_size": str(100 * 1024 ** 3), "physical_utilisation": str(95 * 1024 ** 3), "other_config": {}},
            "S2": {"uuid": "u-s2", "name_label": "TrueNAS", "type": "iso", "content_type": "iso", "shared": True, "physical_size": str(1000 * 1024 ** 3), "physical_utilisation": str(10 * 1024 ** 3), "other_config": {}},
            "S3": {"uuid": "u-s3", "name_label": "NFS VMs", "type": "nfs", "content_type": "user", "shared": True, "physical_size": str(1000 * 1024 ** 3), "physical_utilisation": str(400 * 1024 ** 3), "other_config": {"perfmon": '<config><variable><name value="physical_utilisation"/><alarm_trigger_level value="0.3"/></variable></config>'}},
        }
        self.pbds = {
            "P1": {"uuid": "u-p1", "SR": "S1", "host": "H1", "currently_attached": True},
            "P2": {"uuid": "u-p2", "SR": "S2", "host": "H1", "currently_attached": True},
            "P3": {"uuid": "u-p3", "SR": "S2", "host": "H2", "currently_attached": False},
        }
        self.vmss = {}
        self.vms = {"V1": {"uuid": "u-v1", "name_label": "Home Assistant", "power_state": "Halted"}, "V2": {"uuid": "u-v2", "name_label": "Plex", "power_state": "Running"}}
        outer = self

        class NS:
            pass

        x = NS()
        x.pool = NS()
        x.pool.get_all = lambda: ["POOL"]
        x.pool.get_master = lambda p: "H1"
        x.pool.get_uuid = lambda p: "u-pool"
        x.pool.get_name_label = lambda p: "home"
        x.pool.get_other_config = lambda p: outer.other_config
        x.host = NS()
        x.host.get_uuid = lambda r: outer.hosts[r]["uuid"]
        x.host.get_all_records = lambda: outer.hosts
        x.host.get_enabled = lambda r: True
        x.host.get_name_label = lambda r: outer.hosts[r]["name_label"]
        x.host.get_by_uuid = lambda u: [k for k, v in outer.hosts.items() if v["uuid"] == u][0]
        x.host.call_plugin = lambda host, plugin, fn, args: outer.plugin_calls.append((plugin, fn, args)) or "True"
        x.SR = NS()
        x.SR.get_all_records = lambda: outer.srs
        x.SR.get_name_label = lambda r: outer.srs[r]["name_label"]
        x.SR.get_by_uuid = lambda u: [k for k, v in outer.srs.items() if v["uuid"] == u][0]
        x.PBD = NS()
        x.PBD.get_all_records = lambda: outer.pbds
        x.PBD.plug = lambda r: outer.plugged.append(r)
        x.SM = NS()
        x.SM.get_all_records = lambda: {"SM1": {"type": "ext", "capabilities": ["VDI_CREATE"], "features": {}}}
        x.VMSS = NS()
        x.VMSS.get_all_records = lambda: outer.vmss
        x.VMSS.get_name_label = lambda r: outer.vmss[r]["name_label"]
        x.VMSS.get_by_uuid = lambda u: [k for k, v in outer.vmss.items() if v["uuid"] == u][0]
        x.VM = NS()
        x.VM.get_by_uuid = lambda u: [k for k, v in outer.vms.items() if v["uuid"] == u][0]
        x.VM.get_name_label = lambda r: outer.vms[r]["name_label"]
        x.VM.get_power_state = lambda r: outer.vms[r]["power_state"]
        x.VM.start = lambda r, a, b: outer.started.append(outer.vms[r]["name_label"])
        x.message = NS()
        x.message.create = lambda name, prio, cls, uuid, body: outer.messages.append({"name": name, "priority": prio, "cls": cls, "obj_uuid": uuid, "body": body, "timestamp": xd(time.time())})
        x.message.get_since = lambda since: {str(i): m for i, m in enumerate(outer.messages) if h.xen_epoch(m["timestamp"]) > h.xen_epoch(since)}
        self.x = x


class HelperTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        h.STATE_DIR = self.dir
        h.STATE = os.path.join(self.dir, "state.json")
        h.LOG = os.path.join(self.dir, "log")
        h.LOCK = os.path.join(self.dir, "lock")
        h.this_host_uuid = lambda: "u-h1"
        h.time.sleep = lambda s: None
        self.sent = []
        h.post = lambda url, payload, headers=None, timeout=10: self.sent.append((url, payload, headers)) or 200

    def test_install_script_checks_this_file(self):
        """install.sh's SHA-256 is this svm-helper's: a change to one needs the other."""
        import hashlib, re
        with open(os.path.join(HERE, "..", "svm-helper"), "rb") as f:
            digest = hashlib.sha256(f.read()).hexdigest()
        with open(os.path.join(HERE, "..", "install.sh")) as f:
            m = re.search(r'^SHA256="([0-9a-f]{64})"', f.read(), re.M)
        self.assertIsNotNone(m)
        self.assertEqual(m.group(1), digest, "update SHA256 in helper/install.sh")

    def test_dates(self):
        self.assertEqual(h.xen_epoch("19700101T00:00:10Z"), 10)
        self.assertEqual(h.xen_epoch(h.xen_date(1795000000)), 1795000000)
        self.assertEqual(h.xen_epoch("nonsense"), 0)

    def test_last_expected(self):
        now = time.mktime(datetime.datetime(2026, 11, 20, 19, 30).timetuple())  # a Friday
        daily = h.last_expected("daily", {"hour": "19", "min": "45"}, now)
        self.assertEqual(datetime.datetime.fromtimestamp(daily), datetime.datetime(2026, 11, 19, 19, 45))
        hourly = h.last_expected("hourly", {"min": "15"}, now)
        self.assertEqual(datetime.datetime.fromtimestamp(hourly), datetime.datetime(2026, 11, 20, 19, 15))
        weekly = h.last_expected("weekly", {"hour": "3", "min": "0", "days": "Monday,Wednesday"}, now)
        self.assertEqual(datetime.datetime.fromtimestamp(weekly), datetime.datetime(2026, 11, 18, 3, 0))

    def test_storage_full_once_a_day_and_its_own_limit(self):
        p = Pool()
        done = h.tick(p.x)
        names = [m["body"] for m in p.messages if m["name"] == "SVM_STORAGE_FULL"]
        # local storage over the usual 90%, NFS over the 30% set on it; the ISO library never
        self.assertEqual(len(names), 2, names)
        self.assertTrue(any(n.startswith("Local storage on tico is 95% full") for n in names), names)
        self.assertTrue(any(n.startswith("NFS VMs is 40% full (your limit: 30%)") for n in names), names)
        self.assertTrue(any("95% full" in d for d in done))
        h.tick(p.x)
        self.assertEqual(len([m for m in p.messages if m["name"] == "SVM_STORAGE_FULL"]), 2)
        # storage that tells the pool's monitor itself is left to it
        p.x.SM.get_all_records = lambda: {"SM1": {"type": "ext", "capabilities": ["SR_STATS"], "features": {}}}
        p.messages.clear()
        os.remove(h.STATE)
        h.tick(p.x)
        self.assertEqual(len([m for m in p.messages if m["name"] == "SVM_STORAGE_FULL"]), 1)

    def test_remount_after_a_server_started_before_the_nas(self):
        p = Pool()
        now = time.time()
        # seen mounted on godofredo before it started (an hour ago), not now
        h.save_state({"attached": {"u-p3": now - 7200}})
        h.tick(p.x)
        self.assertEqual(p.plugged, ["P3"])
        self.assertTrue(any(m["body"] == "Mounted 'TrueNAS' again on godofredo" for m in p.messages))
        # never seen mounted: left alone (it may have been taken off on purpose)
        p2 = Pool()
        os.remove(h.STATE)
        h.tick(p2.x)
        self.assertEqual(p2.plugged, [])

    def test_missed_schedule_made_up_once(self):
        p = Pool()
        now = time.time()
        expected = h.last_expected("daily", {"hour": time.strftime("%H", time.localtime(now - 3600)), "min": "0"}, now)
        p.vmss["X1"] = {"uuid": "u-x1", "name_label": "Nightly", "enabled": True, "VMs": ["V1"], "frequency": "daily", "schedule": {"hour": time.strftime("%H", time.localtime(now - 3600)), "min": "0"}, "last_run_time": xd(expected - 86400)}
        h.tick(p.x)
        self.assertEqual(p.plugin_calls, [("vmss", "snapshot_now", {"vmss_uuid": "u-x1"})])
        self.assertTrue(any(m["name"] == "SVM_SCHEDULE_MADE_UP" for m in p.messages))
        h.tick(p.x)
        self.assertEqual(len(p.plugin_calls), 1)
        # run since: nothing to make up; never run: left to XenServer's plugin
        p2 = Pool()
        os.remove(h.STATE)
        p2.vmss["X1"] = dict(p.vmss["X1"], last_run_time=xd(expected + 60))
        p2.vmss["X2"] = dict(p.vmss["X1"], uuid="u-x2", last_run_time="19700101T00:00:00Z")
        h.tick(p2.x)
        self.assertEqual(p2.plugin_calls, [])

    def test_alerts_forwarded_by_priority_and_quiet_hours(self):
        p = Pool()
        p.other_config = {"svm-helper": json.dumps({"channels": {"ntfy": {"on": True, "url": "https://ntfy.sh/home-alerts"}, "discord": {"on": True, "url": "https://discord.example/hook"}}, "min_priority": 3, "watch": {"storage": {"on": False}}})}
        h.tick(p.x)  # the first look starts from now
        self.assertEqual(self.sent, [])
        time.sleep  # (no-op)
        p.messages.append({"name": "VM_CRASHED", "priority": "2", "cls": "VM", "obj_uuid": "u-v1", "body": "", "timestamp": xd(time.time() + 5)})
        p.messages.append({"name": "VM_STARTED", "priority": "5", "cls": "VM", "obj_uuid": "u-v1", "body": "", "timestamp": xd(time.time() + 6)})
        state = h.load_state()
        state["fwd_since"] = time.time() - 10
        h.save_state(state)
        h.tick(p.x)
        self.assertEqual(len(self.sent), 2)  # the crash, to ntfy and Discord; not the start (info)
        url, payload, _ = self.sent[0]
        self.assertEqual(url, "https://ntfy.sh")
        self.assertEqual((payload["topic"], payload["title"]), ("home-alerts", "Home Assistant crashed"))
        self.assertIn("Home Assistant crashed", self.sent[1][1]["content"])

    def test_quiet_hours(self):
        cfg = {"quiet": {"on": True, "from": "23:00", "to": "07:00"}}
        at = lambda hh, mm: time.mktime(datetime.datetime(2026, 11, 20, hh, mm).timetuple())
        self.assertTrue(h.in_quiet_hours(cfg, at(23, 30)))
        self.assertTrue(h.in_quiet_hours(cfg, at(6, 59)))
        self.assertFalse(h.in_quiet_hours(cfg, at(7, 0)))
        self.assertFalse(h.in_quiet_hours({"quiet": {"on": False}}, at(23, 30)))

    def test_start_in_order_once_per_boot(self):
        p = Pool()
        p.other_config = {"svm-helper": json.dumps({"start_order": {"on": True, "vms": [{"uuid": "u-v2", "wait": 5}, {"uuid": "u-v1", "wait": 30}]}, "watch": {"storage": {"on": False}}})}
        h.boot_info = lambda: ("boot-1", 120.0)
        h.tick(p.x)
        self.assertEqual(p.started, ["Home Assistant"])  # Plex was running already
        self.assertTrue(any(m["name"] == "SVM_STARTED_IN_ORDER" for m in p.messages))
        h.tick(p.x)
        self.assertEqual(p.started, ["Home Assistant"])
        # long after the pool started: nothing
        os.remove(h.STATE)
        h.boot_info = lambda: ("boot-2", 99999.0)
        p.started.clear()
        h.tick(p.x)
        self.assertEqual(p.started, [])

    def test_only_the_coordinator_works(self):
        p = Pool()
        h.this_host_uuid = lambda: "u-h2"
        self.assertEqual(h.tick(p.x), ["not the pool's coordinator: it waits"])
        self.assertEqual(p.messages, [])

    def test_alerts_in_words(self):
        self.assertEqual(h.describe("VMSS_SNAPSHOT_FAILED", "Snapshot failed...\nDetails:\nVM: Web Error: x", "Nightly")[0], "Snapshot schedule 'Nightly' couldn't take its snapshots")
        alarm = 'value: 0.970000\nconfig:\n<variable>\n<name value="cpu_usage"/>\n</variable>'
        self.assertEqual(h.describe("ALARM", alarm, "Web Server"), ("Web Server: CPU passed your limit", "now 97%"))
        self.assertEqual(h.describe("SVM_STORAGE_FULL", "Local storage is 95% full", "x"), ("Local storage is 95% full", ""))
        self.assertEqual(h.describe("HOST_SERVER_CERTIFICATE_EXPIRING_07", "", "tico")[0], "tico's certificate expires in 7 days")

    def test_post_sends_json(self):
        got = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                got.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                self.send_response(200)
                self.end_headers()

            def log_message(self, *a):
                pass

        srv = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=srv.handle_request, daemon=True).start()
        real_post = importlib.util.module_from_spec(spec)
        loader.exec_module(real_post)
        self.assertEqual(real_post.post("http://127.0.0.1:%d/hook" % srv.server_port, {"title": "Olá"}), 200)
        srv.server_close()
        self.assertEqual(got, [{"title": "Olá"}])


if __name__ == "__main__":
    unittest.main()
