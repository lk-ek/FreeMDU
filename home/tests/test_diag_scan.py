"""Offline regression tests; no network, appliance or credentials required."""
import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import diag


class ScanTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name) / "scan.json"
        self.calls = []
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(contextlib.redirect_stderr(io.StringIO()))
        self.stack.enter_context(patch.object(diag, "KNOWN_READ_KEYS", ()))
        self.stack.enter_context(patch.object(diag.time, "sleep"))
        self.stack.enter_context(patch.object(diag, "request_with_transient_retry",
                                             return_value="OK software_id=410 hex=0x019a"))
        self.wire = self.stack.enter_context(patch.object(diag, "request_reply", side_effect=self.silent))

    def silent(self, host, port, token, command, first, last, timeout):
        self.calls.append((int(first, 0), int(last, 0), timeout))
        return (f"NO_RESPONSE start={first} end={last} software_id=410 "
                f"scan_version=2 timeout_ms={timeout}")

    def scan(self, timeout=100, recheck=False, excludes=None):
        return diag.scan_read_keys("unused", 3234, "dummy", 0, 3, timeout,
                                   2, self.state, excludes or [], recheck)

    def test_resume_identical_settings(self):
        self.scan()
        self.assertEqual(len(self.calls), 2)
        self.scan()
        self.assertEqual(len(self.calls), 2)

    def test_larger_timeout_repeats_all_keys(self):
        self.scan()
        self.scan(timeout=1000)
        self.assertEqual(self.calls, [(0, 1, 100), (2, 3, 100),
                                     (0, 1, 1000), (2, 3, 1000)])

    def test_explicit_recheck(self):
        self.scan()
        self.scan(recheck=True)
        self.assertEqual(len(self.calls), 4)

    def test_v1_negatives_not_reused(self):
        self.state.write_text(json.dumps({"version": 1, "devices": {
            "software_id:410": {"negative_ranges": [[0, 65535]]}}}))
        self.scan()
        self.assertEqual(len(self.calls), 2)
        state = json.loads(self.state.read_text())
        self.assertEqual(state["version"], 2)
        self.assertIn("legacy_v1", state)

    def test_exclusions_are_not_persisted_as_evidence(self):
        self.scan(excludes=[(0, 1)])
        self.scan()
        self.assertEqual(self.calls, [(2, 3, 100), (0, 1, 100)])

    def test_inconclusive_is_retried(self):
        replies = ["ERR scan_inconclusive partial_read"]
        def reply(*args):
            return replies.pop() if replies else self.silent(*args)
        self.wire.side_effect = reply
        self.scan()
        self.assertEqual(self.wire.call_count, 3)

    def test_repeated_errors_do_not_save_progress(self):
        self.wire.return_value = "ERR scan_inconclusive transient_optical_error"
        self.wire.side_effect = None
        with self.assertRaisesRegex(RuntimeError, "not saved"):
            self.scan()
        self.assertEqual(self.wire.call_count, 3)
        self.assertFalse(self.state.exists())

    def test_old_firmware_and_wrong_device_rejected(self):
        for reply in ["NOT_FOUND start=0x0000 end=0x0001",
                      "OK read_key=0x0000 software_id=999 scan_version=2 confirmed=2",
                      "OK read_key=0x0000 software_id=410 scan_version=2 confirmed=1",
                      "OK read_key=0x0009 software_id=410 scan_version=2 confirmed=2",
                      "NO_RESPONSE start=0x0000 end=0x0003 software_id=410 scan_version=2 timeout_ms=100"]:
            with self.subTest(reply=reply):
                self.wire.side_effect = None
                self.wire.return_value = reply
                with self.assertRaises(RuntimeError):
                    self.scan()
                self.assertFalse(self.state.exists())

    def test_saved_key_always_reconfirmed(self):
        state = {"version": 2, "devices": {"software_id:410": {"read_key": 2}}, "profiles": {}}
        diag.record_silent_range(self.state, state, 410, 100, 0, 3)
        self.wire.side_effect = None
        self.wire.return_value = "OK read_key=0x0002 software_id=410 scan_version=2 confirmed=2"
        self.scan()
        self.assertEqual(self.wire.call_args.args[-3:], ("0x0002", "0x0002", 100))
        self.assertEqual(json.loads(self.state.read_text())["devices"]["software_id:410"]["confirmations"], 2)

    def test_known_candidate_precedes_range(self):
        with patch.object(diag, "KNOWN_READ_KEYS", (3,)):
            self.scan()
        self.assertEqual(self.calls[0], (3, 3, 100))

    def test_invalid_timeout_and_range_rejected(self):
        with self.assertRaises(RuntimeError):
            self.scan(timeout=20)
        self.wire.assert_not_called()

    def test_response_address_and_kind_verified(self):
        with patch.object(diag, "request", return_value="OK kind=eeprom address=0x0000 data=" + "00" * 16):
            with self.assertRaisesRegex(RuntimeError, "mismatch"):
                diag.read_block("unused", 3234, "dummy", "memory", 0, 0)

    def test_shared_registry_contains_reported_key(self):
        rows = diag.READ_KEY_CANDIDATES
        self.assertEqual(len({int(row["key"], 0) for row in rows}), len(rows))
        row = next(row for row in rows if int(row["key"], 0) == 0x2b67)
        self.assertEqual(row["software_ids"], "1998")
        self.assertTrue(row["source"].endswith("/issues/27"))


if __name__ == "__main__":
    unittest.main()
