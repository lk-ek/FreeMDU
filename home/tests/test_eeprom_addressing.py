import contextlib
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import diag


class EepromAddressingTests(unittest.TestCase):
    def test_contiguous_dump_for_byte_and_word_addressed_devices(self):
        for device, unit in [(498, 1), (410, 2)]:
            with self.subTest(device=device), tempfile.TemporaryDirectory() as tmp:
                target = Path(tmp) / 'dump.bin'
                memory = bytes(range(256)) * 2
                calls = []

                def read(host, port, token, kind, key, address, size):
                    calls.append((address, size))
                    return memory[address * unit:address * unit + size]

                with patch.object(diag, 'request_with_transient_retry',
                                  return_value=f'OK software_id={device}'), \
                     patch.object(diag, 'read_block', side_effect=read), \
                     contextlib.redirect_stderr(io.StringIO()):
                    diag.dump_range('esp', 3234, 'test', 'eeprom', 0x2b2c,
                                    0, 255, target)
                    self.assertEqual(target.read_bytes(), memory[:256])
                    # Resume must use the same address conversion.
                    diag.dump_range('esp', 3234, 'test', 'eeprom', 0x2b2c,
                                    0, 383, target)
                    self.assertEqual(target.read_bytes(), memory[:384])
                if device == 498:
                    self.assertEqual(calls, [(i, 16) for i in range(0, 384, 16)])
                else:
                    self.assertEqual(calls, [(0, 128), (64, 128), (128, 128)])


class EepromByteCliTests(unittest.TestCase):
    def test_boundary_addresses_are_sent_without_conversion(self):
        for address in ['0x00ff', '0x0100', '0xffff']:
            output = io.StringIO()
            with patch.object(sys, 'argv', ['diag.py', 'esp', '--token', 'test',
                                          'eeprom1', '0x2b2c', address]), \
                 patch.object(diag, 'request', return_value=
                              f'OK kind=eeprom address={address} data=ff') as request, \
                 contextlib.redirect_stdout(output):
                diag.main()
                self.assertEqual(output.getvalue(), 'ff\n')
                self.assertEqual(request.call_args.args[-3:], ('eeprom1', '0x2b2c', address))
