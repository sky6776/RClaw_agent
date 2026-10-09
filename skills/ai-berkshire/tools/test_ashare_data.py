import subprocess
import sys
import unittest

import ashare_data


class AShareDataTests(unittest.TestCase):
    @staticmethod
    def _quote_raw(timestamp='20260731150000'):
        fields = [''] * 50
        fields[1] = '海澜之家'
        fields[2] = '600398'
        fields[3] = '6.30'
        fields[4] = '6.20'
        fields[5] = '6.22'
        fields[6] = '12345'
        fields[30] = timestamp
        fields[31] = '0.10'
        fields[32] = '1.61'
        fields[33] = '6.35'
        fields[34] = '6.18'
        fields[37] = '123456'
        fields[38] = '1.23'
        fields[39] = '14.00'
        fields[44] = '280.00'
        fields[45] = '302.60'
        fields[46] = '1.65'
        return 'v_sh600398="' + '~'.join(fields) + '";'

    def test_quote_preserves_provider_timestamp(self):
        quote = ashare_data._parse_qq_quote(self._quote_raw())

        self.assertEqual(quote['quote_time'], '2026-07-31 15:00:00 Asia/Shanghai')
        self.assertEqual(quote['quote_time_raw'], '20260731150000')
        self.assertEqual(quote['price'], '6.30')

    def test_quote_rejects_missing_or_malformed_provider_timestamp(self):
        self.assertEqual(ashare_data._parse_qq_quote(self._quote_raw('')), {})
        self.assertEqual(ashare_data._parse_qq_quote(self._quote_raw('2026-07-31')), {})
        self.assertEqual(ashare_data._parse_qq_quote(self._quote_raw('20261340159999')), {})

    def test_cli_help_is_utf8(self):
        completed = subprocess.run(
            [sys.executable, ashare_data.__file__, '--help'],
            capture_output=True,
            text=True,
            encoding='utf-8',
            check=False,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn('A股数据工具', completed.stdout)


if __name__ == '__main__':
    unittest.main()
