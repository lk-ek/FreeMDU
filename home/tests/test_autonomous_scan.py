"""Host CLI uses ESP jobs; no host scan database or long-lived TCP session."""
import contextlib
import hashlib
import io
from pathlib import Path
import struct
import sys
import unittest
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import diag

RUNNING = "OK scan_version=3 state=running software_id=410 next=0x00040 timeout_ms=45"

class AutonomousTests(unittest.TestCase):
    def test_40_ms_is_sent_and_start_returns_immediately(self):
        with patch.object(diag, "request", return_value=RUNNING) as req, \
             patch.object(diag, "load_read_key_scan_state", side_effect=AssertionError("no host state")):
            self.assertEqual(diag.autonomous_start("esp",3234,"dummy",0,65535,40,500), RUNNING)
            req.assert_called_once_with("esp",3234,"dummy","scan-start","0x0000","0xffff",40,500)

    def test_same_start_does_not_reset_adapted_timeout(self):
        with patch.object(diag, "request", return_value=RUNNING) as req:
            diag.autonomous_start("esp",3234,"dummy",0,65535,40,500)
            diag.autonomous_start("esp",3234,"dummy",0,65535,40,500)
            self.assertTrue(all(c.args[3] == "scan-start" for c in req.call_args_list))

    def test_invalid_budget_never_reaches_device(self):
        with patch.object(diag, "request") as req:
            for low,high in [(35,500),(41,500),(40,503),(100,50),(40,2005)]:
                with self.subTest(low=low,high=high), self.assertRaises(RuntimeError):
                    diag.autonomous_start("esp",3234,"dummy",0,65535,low,high)
            req.assert_not_called()

    def test_old_firmware_or_storage_failure_is_rejected(self):
        for reply in ["OK read_key=0x1234 scan_version=2", "OK scan_version=3 state=storage_error"]:
            with patch.object(diag,"request",return_value=reply), self.assertRaises(RuntimeError):
                diag.autonomous_start("esp",3234,"dummy",0,65535,40,500)

    def test_watch_reconnects_without_restarting_job(self):
        replies = [OSError("wifi lost"), RUNNING, "OK scan_version=3 state=found read_key=0x1234"]
        with patch.object(diag,"request",side_effect=replies) as req, \
             patch.object(diag.time,"sleep"), contextlib.redirect_stderr(io.StringIO()), \
             contextlib.redirect_stdout(io.StringIO()):
            diag.watch_scan("esp",3234,"dummy",2)
        self.assertEqual(req.call_count,3)
        self.assertTrue(all(c.args[3] == "scan-status" for c in req.call_args_list))

    def test_partition_table_md5_and_unchanged_existing_partitions(self):
        home=Path(diag.__file__).parent
        old=(home/"tests/partitions-original.bin").read_bytes()
        new=(home/"partitions.bin").read_bytes()
        self.assertEqual(old[:160],new[:160])
        magic,typ,sub,offset,size,name,flags=struct.unpack("<HBBII16sI",new[160:192])
        self.assertEqual((magic,typ,sub,offset,size,name.rstrip(b"\0"),flags),
                         (0x50aa,1,0x40,0x3f0000,0x10000,b"keyscan",0))
        self.assertEqual(new[208:224],hashlib.md5(new[:192]).digest())
        self.assertTrue(all(b == 255 for b in new[224:]))

if __name__ == "__main__": unittest.main()
