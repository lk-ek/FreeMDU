"""CLI targeting must preserve one optical session and parser per appliance."""

import contextlib
import io
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parents[1]))
import diag  # noqa: E402


class TargetTests(unittest.TestCase):
    def test_target_is_in_the_wire_request(self):
        class FakeSocket:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                pass

            def settimeout(self, _timeout):
                pass

            def sendall(self, data):
                self.sent = data

            def makefile(self, *_args, **_kwargs):
                return io.BytesIO(b"OK software_id=410\n")

        fake = FakeSocket()
        with patch.object(diag, "ACTIVE_DIAG_TARGET", "IR2"), \
                patch.object(diag, "connect_with_retry", return_value=fake):
            self.assertEqual(diag.request_reply("example", 3234, "test", "id"), "OK software_id=410")
        self.assertEqual(fake.sent, b"FMDUDIAG1 test IR2 id\n")

    def test_id_defaults_to_both_ports(self):
        observed = []

        def respond(*_args):
            observed.append(diag.ACTIVE_DIAG_TARGET)
            return "OK software_id=410"

        with patch.object(sys, "argv", ["diag.py", "example", "--token", "test", "id"]), \
                patch.object(diag, "request", side_effect=respond), \
                contextlib.redirect_stdout(io.StringIO()):
            diag.main()
        self.assertEqual(observed, ["IR", "IR2"])

    def test_explicit_ir2(self):
        observed = []

        def respond(*_args):
            observed.append(diag.ACTIVE_DIAG_TARGET)
            return "OK software_id=410"

        with patch.object(sys, "argv", ["diag.py", "example", "--token", "test", "id", "--device", "IR2"]), \
                patch.object(diag, "request", side_effect=respond), \
                contextlib.redirect_stdout(io.StringIO()):
            diag.main()
        self.assertEqual(observed, ["IR2"])

    def test_scan_is_single_persistent_job(self):
        with patch.object(sys, "argv", ["diag.py", "example", "--token", "test", "scan-status", "--device", "IR2"]), \
                contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                diag.main()


if __name__ == "__main__":
    unittest.main()
