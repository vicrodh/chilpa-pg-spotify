"""I exercise the packaged executable without a Spotify account or a live profile."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from urllib.parse import parse_qs, urlencode, urlsplit
from urllib.request import urlopen

BINARY = Path(os.environ.get("SPOTIFY_PLUGIN_BINARY", "target/release/plugin")).resolve()


class Protocol(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.process = subprocess.Popen([str(BINARY)], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        self.send(1, "initialize", {"protocol": 1, "data_dir": self.directory.name + "/data",
            "cache_dir": self.directory.name + "/cache", "locale": "es-MX"})
        result = self.receive()["result"]
        self.assertEqual(result["protocol"], 1)
        self.assertTrue(result["capabilities"]["resolve"])
        self.assertTrue(result["capabilities"]["playlist_edit"])

    def tearDown(self):
        self.process.stdin.close()
        self.process.wait(timeout=5)
        self.assertEqual(self.process.stderr.read(), "")
        self.process.stdout.close()
        self.process.stderr.close()
        self.directory.cleanup()

    def send(self, identity, method, params):
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identity,
            "method": method, "params": params}) + "\n")
        self.process.stdin.flush()

    def receive(self):
        # A single line is read per call; subprocess stdout remains bounded.
        return json.loads(self.process.stdout.readline())

    def test_concurrent_requests_do_not_mix_ids(self):
        for identity in range(2, 14):
            self.send(identity, "auth.status", {})
        replies = [self.receive() for _ in range(12)]
        self.assertEqual({row["id"] for row in replies}, set(range(2, 14)))
        self.assertTrue(all(row["result"]["signed_in"] is False for row in replies))

    def test_settings_sign_in_and_cancellation(self):
        self.send(2, "settings.open", {})
        initial = self.receive()["result"]
        revision = initial["revision"]
        self.assertEqual(initial["values"]["callback_url"], "http://127.0.0.1:8989/login")
        self.assertEqual(initial["values"]["client_id"], "")
        self.assertEqual(initial["wizard"]["id"], "client")
        self.send(3, "settings.submit", {"revision": revision, "action": "use_shared", "values": {}})
        form = self.receive()["result"]
        self.assertTrue(form["links"][0]["url"].startswith("https://accounts.spotify.com/authorize?"))
        self.assertEqual(form["values"]["callback_url"], "http://127.0.0.1:8989/login")
        self.assertEqual(form["status"]["phase"], "pending")
        self.send(4, "settings.submit", {"revision": form["revision"], "action": "cancel"})
        self.assertEqual(self.receive()["result"]["wizard"]["id"], "client")

    def test_english_is_default_and_spanish_is_an_explicit_saved_preference(self):
        self.send(2, "settings.open", {})
        english = self.receive()["result"]
        self.assertTrue(english["status"]["label"].startswith("Step 1"))
        self.send(3, "settings.submit", {"revision": english["revision"], "action": "language"})
        spanish = self.receive()["result"]
        self.assertTrue(spanish["status"]["label"].startswith("Paso 1"))
        saved = json.loads((Path(self.directory.name) / "data/account.json").read_text())
        self.assertEqual(saved["language"], "es")
        self.send(4, "settings.submit", {"revision": spanish["revision"], "action": "language"})
        self.assertTrue(self.receive()["result"]["status"]["label"].startswith("Step 1"))

    def test_wizard_recommends_a_personal_client_id_and_validates_it(self):
        self.send(2, "settings.open", {})
        form = self.receive()["result"]
        self.assertEqual(form["wizard"]["step"], 1)
        self.assertTrue(any(action["key"] == "use_personal" for action in form["form"]["actions"]))
        self.send(3, "settings.submit", {"revision": form["revision"], "action": "use_personal", "values": {"client_id": ""}})
        invalid = self.receive()["result"]
        self.assertEqual(invalid["wizard"]["step"], 1)
        self.assertTrue(invalid["notices"])
        self.send(4, "settings.submit", {"revision": invalid["revision"], "action": "use_personal", "values": {"client_id": "a" * 32}})
        library = self.receive()["result"]
        self.assertEqual(library["wizard"]["step"], 2)
        self.assertEqual(parse_qs(urlsplit(library["links"][0]["url"]).query)["client_id"], ["a" * 32])

    def test_browser_callback_is_consumed_by_the_settings_refresh(self):
        self.send(2, "settings.open", {})
        form = self.receive()["result"]
        self.send(3, "settings.submit", {"revision": form["revision"], "action": "use_shared", "values": {}})
        pending = self.receive()["result"]
        query = parse_qs(urlsplit(pending["links"][0]["url"]).query)
        self.assertEqual(query["redirect_uri"], [pending["values"]["callback_url"]])
        address = pending["values"]["callback_url"] + "?" + urlencode({"state": query["state"][0], "error": "access_denied"})
        with urlopen(address, timeout=3) as response:
            self.assertEqual(response.status, 200)
        self.send(4, "settings.open", {})
        completed = self.receive()["result"]
        self.assertEqual(completed["status"]["phase"], "ready")
        self.assertTrue(any("You canceled" in notice for notice in completed["notices"]))

    def test_malformed_input_does_not_break_following_requests(self):
        self.process.stdin.write("{not json\n")
        self.process.stdin.flush()
        self.assertEqual(self.receive()["error"]["code"], -32700)
        self.send(2, "unknown.method", {})
        self.assertEqual(self.receive()["error"]["code"], -32601)
        self.send(3, "auth.status", {})
        self.assertFalse(self.receive()["result"]["signed_in"])

    def test_no_account_is_an_error_instead_of_an_empty_library(self):
        self.send(2, "library.list", {"kind": "albums", "offset": 0, "limit": 50})
        self.assertIn("error", self.receive())
        self.send(3, "track.resolve", {"ref": "track:1234567890123456789012"})
        self.assertIn("error", self.receive())

    def test_playlist_actions_require_real_permissions_and_preserve_error_data(self):
        for identity, action in [(2, "playlist.create"), (3, "playlist.add")]:
            self.send(identity, "page.action", {"action": action, "title": "Mix", "tracks": []})
            error = self.receive()["error"]
            self.assertIn("Enable playlist writing", error["message"])
            self.assertIn("status", error["data"])
            self.assertFalse((Path(self.directory.name) / "data/playlist-requests.json").exists())
        self.send(4, "page.action", {"action": "unsupported"})
        self.assertEqual(self.receive()["error"]["code"], -32601)

    def test_known_rate_limit_reaches_the_host_with_full_retry_deadline(self):
        data = Path(self.directory.name) / "known-limit"
        data.mkdir(mode=0o700)
        client = "a" * 32
        (data / "account.json").write_text(json.dumps({"client_id": client, "user": "test", "name": "Test", "playback": None,
            "access_token": "test-only", "refresh_token": "test-only", "expires": 2**63 - 1}))
        (data / "account.json").chmod(0o600)
        (data / "rate-limit.json").write_text(json.dumps({"version": 1, "client_id": client,
            "until": int(time.time()) + 86400, "routes": {}}))
        with subprocess.Popen([str(BINARY)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True) as child:
            def call(identity, method, params):
                child.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identity,
                    "method": method, "params": params}) + "\n")
                child.stdin.flush()
                return json.loads(child.stdout.readline())
            self.assertIn("result", call(1, "initialize", {"protocol": 1,
                "data_dir": str(data), "cache_dir": str(data / "cache")}))
            error = call(2, "search", {"text": "fixture-only", "limit": 10})["error"]
            self.assertEqual(error["data"]["status"], 429)
            self.assertGreaterEqual(error["data"]["retry_after_ms"], 86390 * 1000)
            self.assertFalse(error["data"]["terminal"])
            saved = json.loads((data / "rate-limit.json").read_text())
            self.assertNotIn("probe_after", saved)
            child.stdin.close()
            child.wait(timeout=5)
            self.assertEqual(child.stderr.read(), "")

    def test_restart_and_logout(self):
        self.send(2, "auth.logout", {})
        self.assertFalse(self.receive()["result"]["signed_in"])
        path = Path(self.directory.name) / "data/account.json"
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.process.stdin.close()
        self.process.wait(timeout=5)
        self.process.stdin = open(os.devnull)
        with subprocess.Popen([str(BINARY)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True) as restarted:
            request = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocol": 1, "data_dir": str(path.parent), "cache_dir": self.directory.name + "/cache"}}
            restarted.stdin.write(json.dumps(request) + "\n")
            restarted.stdin.flush()
            self.assertIn("result", json.loads(restarted.stdout.readline()))
            restarted.stdin.close()
            restarted.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
