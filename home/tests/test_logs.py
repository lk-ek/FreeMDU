"""Live log authentication, configured ports and CLI overrides."""
import contextlib
import io
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import logs


class FakeSocket:
    def __init__(self, response):
        self.response = response
        self.sent = b""

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        pass

    def settimeout(self, value):
        self.timeout = value

    def sendall(self, data):
        self.sent += data

    def makefile(self, *_args, **_kwargs):
        return io.BytesIO(self.response)


class LogTests(unittest.TestCase):
    def test_authenticated_stream_and_disconnect(self):
        fake = FakeSocket(b"OK FreeMDU live logs\nWARN - optical recovery\n")
        output = io.BytesIO()
        with patch.object(logs.socket, "create_connection", return_value=fake), \
                patch.object(logs.sys, "stdout", SimpleNamespace(buffer=output)), \
                contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "device disconnected"):
                logs.stream("example", 3233, "secret", 10)
        self.assertEqual(fake.sent, b"FMDULOG1 secret\n")
        self.assertEqual(output.getvalue(), b"WARN - optical recovery\n")
        self.assertIsNone(fake.timeout)

    def test_rejected_authentication(self):
        fake = FakeSocket(b"ERR authentication\n")
        with patch.object(logs.socket, "create_connection", return_value=fake):
            with self.assertRaisesRegex(RuntimeError, "rejected log connection"):
                logs.stream("example", 3233, "wrong", 10)

    def test_configured_port(self):
        def setting(name):
            return {"OTA_TOKEN": "secret", "LOG_PORT": "4321"}[name]
        with patch.object(sys, "argv", ["logs.py", "example"]), \
                patch.object(logs, "load_config_value", side_effect=setting), \
                patch.object(logs, "stream", side_effect=OSError("offline")) as stream, \
                contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(logs.main(), 1)
        stream.assert_called_once_with("example", 4321, "secret", 10.0)

    def test_explicit_port_and_token_need_no_local_config(self):
        with patch.object(sys, "argv", ["logs.py", "example", "--port", "5678", "--token", "override"]), \
                patch.object(logs, "load_config_value", side_effect=RuntimeError("missing")) as config, \
                patch.object(logs, "stream", side_effect=OSError("offline")) as stream, \
                contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(logs.main(), 1)
        config.assert_not_called()
        stream.assert_called_once_with("example", 5678, "override", 10.0)

    def test_invalid_port(self):
        with patch.object(sys, "argv", ["logs.py", "example", "--port", "0", "--token", "secret"]), \
                contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                logs.main()


if __name__ == "__main__":
    unittest.main()
