import contextlib
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import diag

class FullReadTests(unittest.TestCase):
    def test_full_key_sent_for_each_read_size(self):
        for kind,size in [('memory',16),('memory',128),('eeprom',1),('eeprom',16),('eeprom',128)]:
            width = 4 if kind == 'eeprom' else 8
            reply = f'OK kind={kind} address=0x{0x100:0{width}x} data=' + 'ab'*size
            with self.subTest(kind=kind,size=size), patch.object(diag,'request',return_value=reply) as req:
                self.assertEqual(diag.read_block('esp',3234,'token',kind,0x2b2c,0x100,size,0x0f2f),bytes([0xab])*size)
                command = f'full-{"eeprom" if kind == "eeprom" else "mem"}{size}'
                req.assert_called_once_with('esp',3234,'token',command,'0x2b2c',f'0x{0x100:0{width}x}','0x0f2f')

    def test_single_byte_cli(self):
        with patch.object(sys,'argv',['diag.py','esp','--token','token','eeprom1','0x2b2c','0x0100','--full-key','0x0f2f']), patch.object(diag,'request',return_value='OK kind=eeprom address=0x0100 data=ff') as req, contextlib.redirect_stdout(io.StringIO()) as out:
            diag.main()
            self.assertEqual(out.getvalue(),'ff\n')
            self.assertEqual(req.call_args.args[-1],'0x0f2f')

    def test_dump_reuses_full_key_for_every_block(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(diag,'read_block',return_value=bytes(128)) as read, contextlib.redirect_stderr(io.StringIO()):
            diag.dump_range('esp',3234,'token','memory',0x2b2c,0,255,Path(tmp)/'dump.bin',full_key=0x0f2f)
            self.assertEqual(read.call_count,2)
            self.assertTrue(all(c.kwargs == {'full_key':0x0f2f} for c in read.call_args_list))
