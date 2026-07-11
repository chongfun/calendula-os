#!/usr/bin/env python3
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import bench


class BenchParserTests(unittest.TestCase):
    def test_parse_structured_render(self) -> None:
        event = bench.parse_line(
            "bench: render view=Reading mode=Fast page=12 chapter=5 layout_ms=22 flush_ms=438 prestage_ms=15 t_ms=96260",
            "page-turn",
        )[0]
        self.assertEqual(event["event"], "render")
        self.assertEqual(event["view"], "Reading")
        self.assertEqual(event["mode"], "Fast")
        self.assertEqual(event["flush_ms"], 438)

    def test_parse_legacy_render(self) -> None:
        event = bench.parse_line(
            "bench: render Reading Fast page=12 ch=5 layout=24ms flush=438ms prestage=16ms t=93958",
            "page-turn",
        )[0]
        self.assertEqual(event["event"], "render")
        self.assertEqual(event["view"], "Reading")
        self.assertEqual(event["mode"], "Fast")
        self.assertTrue(event["legacy"])

    def test_button_normalization(self) -> None:
        event = bench.parse_line(
            "bench: input button=Some(Next) aux=2061 nav=5 page_raw=2937 t_ms=10524",
            "page-turn",
        )[0]
        self.assertEqual(event["button"], "Next")


class BenchReportTests(unittest.TestCase):
    def test_page_turn_duration_pairs_input_with_next_reading_render(self) -> None:
        events = [
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 100},
            {
                "suite": "page-turn",
                "event": "render",
                "view": "Reading",
                "mode": "Fast",
                "t_ms": 560,
            },
        ]
        self.assertEqual(bench.page_turn_durations(events), [460])

    def test_budget_warning_for_slow_page_turn(self) -> None:
        events = [
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 100},
            {
                "suite": "page-turn",
                "event": "render",
                "view": "Reading",
                "mode": "Fast",
                "t_ms": 800,
            },
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertTrue(any("page-turn median" in warning for warning in warnings))

    def test_suite_signal_warning_for_empty_capture(self) -> None:
        warnings = bench.evaluate_suite_signals(
            [
                {"suite": "storage-cache", "event": "run_start"},
                {"suite": "storage-cache", "event": "run_end"},
            ]
        )
        self.assertEqual(warnings, ["storage-cache: no parsed bench telemetry"])

    def test_strict_report_uses_suite_signal_validation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "empty.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"suite": "storage-cache", "event": "run_start"}),
                        json.dumps({"suite": "storage-cache", "event": "run_end"}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            warnings = bench.summarize_paths([path], None, validate_suites=True)
        self.assertEqual(warnings, ["storage-cache: no parsed bench telemetry"])


class CaptureLinesTests(unittest.TestCase):
    @patch("bench.serial_lines")
    def test_initial_oserror_propagates(self, mock_serial) -> None:
        import errno
        def gen():
            raise OSError(errno.ENOENT, "Not found")
            yield
        mock_serial.return_value = gen()
        with self.assertRaises(OSError):
            list(bench.capture_lines("/dev/port"))

    @patch("bench.print")
    @patch("bench.time.sleep")
    @patch("bench.os.path.exists")
    @patch("bench.serial_lines")
    def test_reconnects_after_oserror(self, mock_serial, mock_exists, mock_sleep, mock_print) -> None:
        import errno
        from unittest.mock import call
        def gen1():
            yield ""
            yield "data\n"
            raise OSError(errno.ENODEV, "Vanished")
        def gen2():
            yield ""
            yield "more data\n"
        mock_serial.side_effect = [gen1(), gen2()]
        mock_exists.side_effect = [False, True]
        
        lines = list(bench.capture_lines("/dev/port"))
        
        self.assertEqual(lines, ["data\n", "more data\n"])
        mock_sleep.assert_has_calls([call(0.5), call(0.5)])
        mock_print.assert_any_call("port: /dev/port vanished (device asleep?); wake it to resume capture", flush=True)
        mock_print.assert_any_call("port: back; resuming capture", flush=True)

    @patch("bench.print")
    @patch("bench.time.sleep")
    @patch("bench.time.monotonic")
    @patch("bench.os.path.exists")
    @patch("bench.serial_lines")
    def test_stop_at_expiration_while_absent(self, mock_serial, mock_exists, mock_monotonic, mock_sleep, mock_print) -> None:
        import errno
        def gen():
            yield ""
            yield "data\n"
            raise OSError(errno.ENODEV, "Vanished")
        mock_serial.return_value = gen()
        mock_exists.return_value = False
        mock_monotonic.return_value = 100.0
        
        lines = list(bench.capture_lines("/dev/port", stop_at=50.0))
        
        self.assertEqual(lines, ["data\n"])
        mock_print.assert_any_call("port: capture window ended while the device was away", flush=True)
        mock_sleep.assert_not_called()


if __name__ == "__main__":
    unittest.main()
