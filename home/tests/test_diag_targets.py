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

            def settimeout(self, timeout):
                self.timeout = timeout

            def sendall(self, data):
                self.sent = data

            def makefile(self, *_args, **_kwargs):
                return io.BytesIO(b"OK software_id=410\n")

        fake = FakeSocket()
        with patch.object(diag, "ACTIVE_DIAG_TARGET", "IR2"), \
                patch.object(diag, "connect_with_retry", return_value=fake):
            self.assertEqual(diag.request_reply("example", 3234, "test", "id"), "OK software_id=410")
        self.assertEqual(fake.sent, b"FMDUDIAG1 test IR2 id\n")
        self.assertEqual(fake.timeout, 180)

    def test_transport_errors_are_transient_but_rejected_keys_are_not(self):
        for reply in (
            "ERR noisy_line", "ERR read_memory Io(Receive(GlitchOccurred))",
            "ERR read_eeprom IncorrectChecksum", "ERR query_max_baud_rate EchoMismatch",
            "ERR unlock_read_access transport", "ERR unlock_read_access timeout",
        ):
            with self.subTest(reply=reply), patch.object(diag, "request_reply", return_value=reply):
                with self.assertRaises(diag.DiagnosticTransientError):
                    diag.request("example", 3234, "test", "id")
        with patch.object(diag, "request_reply", return_value="ERR unlock_read_access"):
            with self.assertRaises(RuntimeError) as caught:
                diag.request("example", 3234, "test", "mem16")
            self.assertNotIsInstance(caught.exception, diag.DiagnosticTransientError)

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
