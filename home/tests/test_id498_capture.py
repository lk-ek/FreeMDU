import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
import diag

class CaptureTests(unittest.TestCase):
    def test_capture_records_three_blocks_and_flushes_on_ctrl_c(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'run.jsonl'
            with patch.object(diag,'request_with_transient_retry',return_value='OK software_id=498'), patch.object(diag,'request',return_value='OK software_id=498'), patch.object(diag,'read_block',return_value=bytes(16)) as read, patch.object(diag.time,'sleep',side_effect=KeyboardInterrupt), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                diag.capture_id498('esp',3234,'token',path,5)
            record = json.loads(path.read_text())
            self.assertEqual(set(record['blocks']),{'0x00b0','0x0260','0x0270'})
            self.assertEqual([c.args[-1] for c in read.call_args_list],[0xb0,0x260,0x270])

    def test_changed_device_does_not_get_memory_requests(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'run.jsonl'
            with patch.object(diag,'request_with_transient_retry',return_value='OK software_id=498'), patch.object(diag,'request',return_value='OK software_id=410'), patch.object(diag,'read_block') as read, patch.object(diag.time,'sleep',side_effect=KeyboardInterrupt), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                diag.capture_id498('esp',3234,'token',path,5)
            self.assertIn('error',json.loads(path.read_text()))
            read.assert_not_called()
