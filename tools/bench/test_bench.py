#!/usr/bin/env python3
import json
import re
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

    def test_parse_prestage_event(self) -> None:
        event = bench.parse_line(
            "bench: prestage staged=true elapsed_ms=24 t_ms=96284",
            "page-turn",
        )[0]
        self.assertEqual(event["event"], "prestage")
        self.assertEqual(event["elapsed_ms"], 24)
        self.assertTrue(event["staged"])

    def test_prestage_values_prefers_standalone_events(self) -> None:
        events = [
            {"event": "render", "view": "Reading", "layout_ms": 13, "t_ms": 100},
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "t_ms": 124},
        ]
        self.assertEqual(bench.prestage_values(events), [24])

    def test_prestage_values_falls_back_to_pre_split_render_events(self) -> None:
        """Captures from before the render/prestage split still summarize."""
        events = [
            {"event": "render", "view": "Reading", "prestage_ms": 28, "t_ms": 100},
            {"event": "render", "view": "Reading", "prestage_ms": 27, "t_ms": 200},
        ]
        self.assertEqual(bench.prestage_values(events), [28, 27])

    def test_prestage_values_combines_standalone_and_legacy_render_events(self) -> None:
        """Log containing both firmware event shapes combines samples in ordering."""
        events = [
            {"event": "render", "view": "Reading", "prestage_ms": 28, "t_ms": 100},
            {"event": "render", "view": "Reading", "layout_ms": 13, "t_ms": 200},
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "t_ms": 224},
        ]
        self.assertEqual(bench.prestage_values(events), [28, 24])

    def test_page_turn_duration_ends_at_the_render_event(self) -> None:
        """press-to-settled must not absorb the prestage that follows it."""
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1424},
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "t_ms": 1448},
        ]
        self.assertEqual(bench.page_turn_durations(events), [424])

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

    def test_parse_deep_sleep_wake_line(self) -> None:
        event = bench.parse_line(
            "main: deep_sleep_wake=true (gpio=true, sleep_image=true)",
            "sleep-sync",
        )[0]
        self.assertEqual(event["event"], "boot")
        self.assertTrue(event["deep_sleep_wake"])
        self.assertTrue(event["gpio"])
        self.assertTrue(event["sleep_image"])

    def test_parse_cold_boot_line(self) -> None:
        event = bench.parse_line(
            "main: deep_sleep_wake=false (gpio=false, sleep_image=false)",
            "sleep-sync",
        )[0]
        self.assertEqual(event["event"], "boot")
        self.assertFalse(event["gpio"])

    def test_parse_boot_stage_line(self) -> None:
        event = bench.parse_line("main: spawn display t_ms=142", "sleep-sync")[0]
        self.assertEqual(event["event"], "boot_stage")
        self.assertEqual(event["stage"], "main: spawn display")
        self.assertEqual(event["t_ms"], 142)

    def test_boot_stage_line_without_t_ms_is_ignored(self) -> None:
        """Logs from firmware predating the t_ms stamps must not parse."""
        self.assertEqual(bench.parse_line("main: spawn display", "sleep-sync"), [])
        self.assertEqual(bench.parse_line("sd: session enter", "page-turn"), [])


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

    @patch("builtins.print")
    def test_summarize_paths_default_reports_latest_run(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"suite": "run1", "event": "run_start"}),
                        json.dumps({"suite": "run1", "event": "render", "view": "Reading", "mode": "Fast"}),
                        json.dumps({"suite": "run2", "event": "run_start"}),
                        json.dumps({"suite": "run2", "event": "refresh", "busy_ms": 100}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
            
            # Since latest_only=True, we should only see run2's events.
            mock_print.assert_any_call("events:        2")
            mock_print.assert_any_call("renders:       0")
            mock_print.assert_any_call("bench report: latest run only (run2; 1 earlier run(s) in the log — pass --all to pool)")

    @patch("builtins.print")
    def test_summarize_paths_all_reports_every_run(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"suite": "run1", "event": "run_start"}),
                        json.dumps({"suite": "run1", "event": "render", "view": "Reading", "mode": "Fast"}),
                        json.dumps({"suite": "run2", "event": "run_start"}),
                        json.dumps({"suite": "run2", "event": "refresh", "busy_ms": 100}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None, latest_only=False)
            
            # Since latest_only=False, we should see both run1 and run2's events.
            mock_print.assert_any_call("events:        4")
            mock_print.assert_any_call("renders:       1")

class BudgetLoadingTests(unittest.TestCase):
    def test_none_path_is_intentionally_disabled(self) -> None:
        self.assertEqual(bench.load_budgets(None), ({}, None))

    def test_missing_parser_reports_problem(self) -> None:
        with patch.object(bench, "tomllib", None):
            budgets, problem = bench.load_budgets(Path("benches.toml"))
        self.assertEqual(budgets, {})
        self.assertIn("tomllib", problem)
        self.assertIn("tomli", problem)

    def test_missing_file_reports_problem(self) -> None:
        budgets, problem = bench.load_budgets(Path("does-not-exist.toml"))
        self.assertEqual(budgets, {})
        self.assertIsNotNone(problem)

    def _write_minimal_log(self, tmp: str) -> Path:
        path = Path(tmp) / "log.jsonl"
        path.write_text(
            json.dumps(
                {"suite": "page-turn", "event": "render", "view": "Reading", "t_ms": 100}
            )
            + "\n",
            encoding="utf-8",
        )
        return path

    def test_strict_report_fails_loudly_when_budgets_unloadable(self) -> None:
        """--strict with no TOML parser must exit non-zero, not verify nothing."""
        with tempfile.TemporaryDirectory() as tmp:
            log = self._write_minimal_log(tmp)
            budgets = Path(tmp) / "benches.toml"
            budgets.write_text("[page-turn]\n", encoding="utf-8")
            with patch.object(bench, "tomllib", None):
                with self.assertRaises(SystemExit) as ctx:
                    bench.summarize_paths([log], budgets, validate_suites=True)
        self.assertIn("--strict cannot enforce budgets", str(ctx.exception))

    @patch("builtins.print")
    def test_non_strict_report_warns_when_budgets_unloadable(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            log = self._write_minimal_log(tmp)
            budgets = Path(tmp) / "benches.toml"
            budgets.write_text("[page-turn]\n", encoding="utf-8")
            with patch.object(bench, "tomllib", None):
                warnings = bench.summarize_paths([log], budgets)
        self.assertEqual(warnings, [])
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("budgets not checked", printed)

    @patch("builtins.print")
    def test_strict_report_flags_empty_log(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "empty.jsonl"
            path.write_text("", encoding="utf-8")
            warnings = bench.summarize_paths([path], None, validate_suites=True)
        self.assertEqual(warnings, ["no events parsed"])

    def test_load_budgets_uses_whatever_parser_is_bound(self) -> None:
        """The `tomli` fallback is advertised in the README and the --strict
        error message, so it must actually be consumed. The import-time
        branch cannot be exercised here, but the risk it carries can: that
        the fallback binds and `load_budgets` ignores it.
        """

        class FakeParser:
            def __init__(self) -> None:
                self.calls = 0

            def load(self, handle) -> dict:
                self.calls += 1
                return {"page-turn": {"median_press_to_settled_ms": 42}}

        parser = FakeParser()
        with patch.object(bench, "tomllib", parser):
            budgets, problem = bench.load_budgets(bench.DEFAULT_BUDGETS)
        self.assertIsNone(problem)
        self.assertEqual(parser.calls, 1)
        self.assertEqual(budgets["page-turn"]["median_press_to_settled_ms"], 42)

    def test_checked_in_budgets_have_no_dead_keys(self) -> None:
        """Every key in benches.toml must be read somewhere in bench.py.

        Scans the TOML textually rather than parsing it, so this runs on the
        interpreter `tools/check.sh` actually invokes. Skipping here on 3.9
        meant the dead-key guard did not run under the repo's own python --
        the same "gate that is silently absent" shape this file exists to
        stop. A dead key is a budget that reads as enforced and is not, which
        is worse than no budget at all.
        """
        text = bench.DEFAULT_BUDGETS.read_text(encoding="utf-8")
        source = Path(bench.__file__).read_text(encoding="utf-8")
        keys = re.findall(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=", text, re.MULTILINE)
        self.assertTrue(keys, "no budget keys found -- the scan itself is broken")
        for key in keys:
            self.assertIn(f'"{key}"', source, f"budget key {key} is read by nothing")


class PageTurnTrustTests(unittest.TestCase):
    def _burst_events(self) -> list:
        # Operator triple-taps during one refresh. The render settles at 1500
        # having begun at 1500-10-400 = 1090, so it answers the two presses
        # that preceded it (one coalesced) and the third is still unanswered
        # when the capture ends. Both buckets, one fixture.
        return [
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 1000},
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 1080},
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 1160},
            {
                "suite": "page-turn",
                "event": "render",
                "view": "Reading",
                "t_ms": 1500,
                "layout_ms": 10,
                "flush_ms": 400,
            },
        ]

    def test_burst_capture_is_untrusted(self) -> None:
        stats = bench.page_turn_stats(self._burst_events())
        # Measured from the newest press the render answered (1080), not the
        # oldest — charging the oldest is what made a burst look like a slow
        # device rather than a fast operator.
        self.assertEqual(stats.durations, [420])
        self.assertEqual(stats.presses, 3)
        self.assertEqual(stats.coalesced_presses, 1)
        self.assertEqual(stats.unmatched_presses, 1)
        self.assertEqual(stats.untrusted_presses, 2)
        self.assertFalse(stats.median_trusted)

    def test_a_repaint_is_not_charged_to_the_next_press(self) -> None:
        """The defect this pairing exists to prevent.

        Until #56 the firmware repainted twice per turn. Under render-driven
        FIFO pairing the second repaint consumed the *following* press and
        credited it with a near-zero duration, which is where the real
        fixture's 2 ms samples came from. The repaint begins before that
        press lands, so it must answer nothing.
        """
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            # The turn the reader actually sees.
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 1500,
                "layout_ms": 10,
                "flush_ms": 400,
            },
            {"event": "input", "button": "Next", "t_ms": 1600},
            # Redundant repaint: began at 1650-10-400 = 1240, before the
            # second press existed, so it cannot be that press's frame.
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 1650,
                "layout_ms": 10,
                "flush_ms": 400,
            },
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 2100,
                "layout_ms": 10,
                "flush_ms": 400,
            },
        ]
        stats = bench.page_turn_stats(events)
        self.assertEqual(stats.durations, [500, 500])
        self.assertEqual(stats.reading_renders, 3)
        self.assertEqual(stats.coalesced_presses, 0)
        self.assertEqual(stats.unmatched_presses, 0)
        self.assertTrue(stats.median_trusted)

    def test_deliberate_capture_is_trusted(self) -> None:
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1500},
            {"event": "input", "button": "Next", "t_ms": 3000},
            {"event": "render", "view": "Reading", "t_ms": 3500},
        ]
        stats = bench.page_turn_stats(events)
        self.assertEqual(stats.durations, [500, 500])
        self.assertEqual(stats.unmatched_presses, 0)
        self.assertTrue(stats.median_trusted)

    def test_untrusted_median_is_not_checked_against_budget(self) -> None:
        warnings = bench.evaluate_budgets(
            self._burst_events(),
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertTrue(any("untrusted" in warning for warning in warnings))
        self.assertFalse(any("above warning budget" in warning for warning in warnings))

    def test_budget_floor_warns_on_suspiciously_fast_median(self) -> None:
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1030},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"median_press_to_settled_min_ms": 250}},
        )
        self.assertTrue(any("below plausibility floor" in warning for warning in warnings))

    def test_suite_signals_flag_untrusted_page_turn(self) -> None:
        warnings = bench.evaluate_suite_signals(self._burst_events())
        self.assertTrue(
            any("produced no page turn" in warning for warning in warnings)
        )

    @patch("builtins.print")
    def test_report_suppresses_untrusted_median(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(json.dumps(event) for event in self._burst_events()) + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("UNTRUSTED", printed)
        self.assertNotIn("page turn      median=", printed)
        self.assertIn("presses=3 page_turns=1", printed)
        self.assertIn("coalesced=1", printed)
        self.assertIn("unmatched=1", printed)


class PageTurnEpochTests(unittest.TestCase):
    """Pairing must not reach across a device time epoch.

    `t_ms` is device uptime: it restarts at every reboot, and each capture in
    a pooled log carries its own. Sorting globally by it interleaves clocks.
    """

    def test_two_runs_do_not_pair_across_each_other(self) -> None:
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1500},
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1200},
            {"event": "render", "view": "Reading", "t_ms": 1700},
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        # Globally sorted these are press,press,render,render: the first
        # render answers both, inventing a coalesced press and a 300 ms turn.
        self.assertEqual(stats.durations, [500, 500])
        self.assertEqual(stats.coalesced_presses, 0)
        self.assertEqual(stats.unmatched_presses, 0)

    def test_runs_are_split_even_when_uptime_never_goes_backwards(self) -> None:
        """The run split is load-bearing on its own, not just via reboots.

        Two captures against a device that was never reset have increasing
        `t_ms` across the boundary, so no regression marks it and the boot
        segmentation cannot see it. Here the first capture ends immediately
        after a press — the operator stopped recording — and without the run
        split that press is answered by the *next capture's* first render,
        inventing a cross-capture duration out of nothing.
        """
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "run_start", "suite": "page-turn"},
            {"event": "render", "view": "Reading", "t_ms": 2000},
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [])
        self.assertEqual(stats.unmatched_presses, 1)

    def test_a_reboot_inside_one_run_splits_the_pairing(self) -> None:
        events = [
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1500},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 1600},
            # Woken: uptime restarts and overlaps the pre-sleep range.
            {"event": "input", "button": "Next", "t_ms": 1200},
            {"event": "render", "view": "Reading", "t_ms": 1700},
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [500, 500])
        self.assertEqual(stats.coalesced_presses, 0)


class RenderBeginTests(unittest.TestCase):
    """`req_ms` is the boundary; the subtraction is only a fallback.

    The firmware stamps `req_ms` as it dequeues the render command, before any
    preparation. Inferring the start as `t_ms - layout_ms - flush_ms` omits
    the work outside those two intervals — catalog and TOC reads ahead of
    layout, and after the flush a framebuffer copy plus a chapter-tracking
    read that touches the card at chapter boundaries. Every omission pushes
    the estimate *later* than the truth, which is the direction that mispairs.
    """

    def _render(self, **over) -> dict:
        event = {
            "event": "render",
            "view": "Reading",
            "t_ms": 2000,
            "layout_ms": 10,
            "flush_ms": 400,
        }
        event.update(over)
        return event

    def test_req_ms_wins_over_the_inferred_start(self) -> None:
        self.assertEqual(bench.render_begin_ms(self._render(req_ms=1500)), 1500)

    def test_the_inferred_start_is_the_fallback_for_older_logs(self) -> None:
        self.assertEqual(bench.render_begin_ms(self._render()), 1590)

    def test_a_press_during_the_queue_wait_belongs_to_the_next_frame(self) -> None:
        """A render can wait in the channel behind the display task's work.

        `req_ms` is stamped when the app freezes the state, not when the
        display task dequeues. Frame A was frozen at 1100 and did not reach
        the panel pipeline until 1500, so the press at 1200 arrived while A
        was already queued and immutable: it belongs to B. Pairing on the
        dequeue instant would have charged it to A and reported a 700 ms
        turn instead of A's real 800 ms and B's 1100 ms.
        """
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "input", "button": "Next", "t_ms": 1200},
            self._render(req_ms=1100, deq_ms=1500, t_ms=1900),
            self._render(req_ms=1950, deq_ms=1950, t_ms=2300),
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [900, 1100])
        self.assertEqual(stats.coalesced_presses, 0)
        self.assertEqual(stats.unmatched_presses, 0)

    def test_pairing_survives_past_the_32_bit_uptime_horizon(self) -> None:
        """`requested_at_ms` is a `u64`, so there is no wrap to reconstruct.

        Held as a boundary case because it was nearly a `u32`. Truncating at
        49.7 days would have restarted `req_ms` near zero while the press and
        settle timestamps kept counting, so every press after the wrap looked
        later than the render it belonged to, went unanswered, and eventually
        suppressed the median or failed `--strict` — on long soaks, which is
        the run most likely to reach it and least likely to be re-run.
        """
        wrap = 2**32
        events = [
            {"event": "input", "button": "Next", "t_ms": wrap - 500},
            self._render(req_ms=wrap - 400, deq_ms=wrap - 300, t_ms=wrap + 100),
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [600])
        self.assertEqual(stats.unmatched_presses, 0)

    def test_an_unstamped_request_falls_back(self) -> None:
        """0 means unstamped, not "frozen at boot"."""
        self.assertEqual(bench.render_begin_ms(self._render(req_ms=0)), 1590)

    def test_a_press_after_the_request_was_frozen_is_not_credited(self) -> None:
        """The defect `req_ms` exists to close.

        The press lands at 1550: after the request was frozen at 1500, but
        before the inferred start of 1590. It cannot be in this frame, yet
        the subtraction would credit it with a 450 ms turn.
        """
        events = [
            {"event": "input", "button": "Next", "t_ms": 1550},
            self._render(req_ms=1500),
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [])
        self.assertEqual(stats.unmatched_presses, 1)

        without = [
            {"event": "input", "button": "Next", "t_ms": 1550},
            self._render(),
        ]
        self.assertEqual(bench.page_turn_stats_over_epochs(without).durations, [450])


class BudgetCoverageTests(unittest.TestCase):
    """A budget with nothing to measure must not read as enforced."""

    def test_a_configured_budget_with_no_samples_warns(self) -> None:
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1500, "layout_ms": 12},
            # No refresh events at all, so the Fast-refresh budget covers
            # nothing -- and used to pass --strict while covering nothing.
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"fast_refresh_busy_warn_ms": 500}},
        )
        self.assertTrue(
            any("fast_refresh_busy_warn_ms" in w and "nothing was measured" in w for w in warnings),
            warnings,
        )

    def test_a_budget_with_samples_does_not_warn_about_coverage(self) -> None:
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "refresh", "mode": "Fast", "busy_ms": 380},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"fast_refresh_busy_warn_ms": 500}},
        )
        self.assertFalse(any("nothing was measured" in w for w in warnings), warnings)

    def test_another_suites_budgets_are_not_faulted(self) -> None:
        """A page-turn capture holds no storage telemetry, and should not be
        told off for it -- otherwise every report warns about every suite."""
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "refresh", "mode": "Fast", "busy_ms": 380},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {
                "page-turn": {"fast_refresh_busy_warn_ms": 500},
                "storage-cache": {"catalog_load_warn_ms": 500},
            },
        )
        self.assertFalse(any("catalog_load_warn_ms" in w for w in warnings), warnings)


class StorageBuildReportTests(unittest.TestCase):
    @patch("builtins.print")
    def test_report_summarizes_build_telemetry(self, mock_print) -> None:
        events = [
            {
                "suite": "storage-cache",
                "event": "storage_build",
                "elapsed_ms": 14948,
                "spine_ms": 13871,
                "write_ms": 4340,
                "sections": 51,
                "pages": 441,
                "rd_calls": 3026,
                "rd_blocks": 3026,
                "wr_calls": 2000,
                "wr_blocks": 2000,
                "key": "E3C2056B",
            },
            {
                "suite": "storage-cache",
                "event": "storage_build",
                "elapsed_ms": 14147,
                "spine_ms": 13000,
                "write_ms": 4200,
                "sections": 51,
                "pages": 441,
                "rd_calls": 3017,
                "rd_blocks": 3017,
                "wr_calls": 1990,
                "wr_blocks": 1990,
                "key": "E3C2056B",
            },
            {
                "suite": "storage-cache",
                "event": "storage_first_page",
                "elapsed_ms": 900,
                "pages": 12,
                "sections": 1,
                "key": "E3C2056B",
            },
            {
                "suite": "storage-cache",
                "event": "storage_background_build",
                "book_id": 5,
                "pages": 441,
                "elapsed_ms": 60000,
            },
        ]
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(json.dumps(event) for event in events) + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("storage build  median=14548ms", printed)
        self.assertIn("build spine    median=13436ms", printed)
        self.assertIn("build write    median=4270ms", printed)
        self.assertIn(
            "build io:      builds=2 rd_calls=6043 rd_blocks=6043 wr_calls=3990 wr_blocks=3990",
            printed,
        )
        self.assertIn("first page     median=900ms", printed)
        self.assertIn("bg build       median=60000ms", printed)


class BootSegmentTests(unittest.TestCase):
    def test_unexpected_regression_warns(self) -> None:
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "reset"])
        self.assertEqual(len(warnings), 1)
        self.assertIn("t_ms went backwards", warnings[0])

    def test_wake_after_deep_sleep_does_not_warn(self) -> None:
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 61000},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "wake"])
        self.assertEqual(warnings, [])

    def test_boot_marker_splits_without_warning(self) -> None:
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "boot", "deep_sleep_wake": True, "gpio": True, "sleep_image": True},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "wake"])
        self.assertEqual(warnings, [])

    def test_failed_panel_sleep_is_not_a_wake(self) -> None:
        """`phase=complete` prints on both outcomes, carrying the result in `ok`.

        When the panel handshake fails the power task deliberately stays
        awake and retries, so a reboot after it is a genuine unexplained
        reset. Latching on the sleep line alone relabelled it a wake and
        suppressed the warning — in `reader-soak` and `sleep-sync`, where
        sleeps are frequent, that suppressed the check across most of a run.
        """
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "sleep", "phase": "complete", "ok": False, "t_ms": 61000},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "reset"])
        self.assertEqual(len(warnings), 1)

    def test_activity_after_a_terminal_sleep_clears_the_latch(self) -> None:
        """A sleep that reported success but was followed by more work."""
        run = [
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 61000},
            # The device kept running, so it never deep-slept.
            {"event": "render", "view": "Reading", "t_ms": 80000},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "reset"])
        self.assertEqual(len(warnings), 1)

    def test_wake_without_a_retained_sleep_image_counts_as_cold(self) -> None:
        """A wake that lost its panel image pays the full waveform.

        Its first paint belongs with the cold cluster; pooling it with fast
        wakes hides exactly the case the wake path exists to avoid.
        """
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "boot", "deep_sleep_wake": True, "gpio": True, "sleep_image": False},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, _ = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "cold"])

    def test_small_t_ms_inversion_is_skew_not_a_reboot(self) -> None:
        """Interrupt-priority input can print out of order by a hair.

        A real reboot restarts the uptime clock, so its regression is the
        whole session. Without a guard a 1 ms inversion fabricated a boot,
        a boot-to-first-paint sample, and a --strict failure.
        """
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "input", "button": "Next", "t_ms": 59999},
            {"event": "render", "view": "Reading", "t_ms": 60500},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach"])
        self.assertEqual(warnings, [])

    def test_boot_paints_are_reported_per_kind(self) -> None:
        """A cold boot and a wake must never share a median."""
        run = [
            {"event": "boot", "deep_sleep_wake": False, "gpio": False, "sleep_image": False},
            {"event": "render", "view": "Home", "t_ms": 3310},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
            {"event": "render", "view": "Reading", "t_ms": 690},
        ]
        paints, _, _ = bench.boot_report([run])
        self.assertEqual(paints, {"cold": [3310], "wake": [690]})

    def test_sd_session_enter_is_not_a_boot_stage(self) -> None:
        """It fires several times before first paint, so its median is noise."""
        self.assertIsNone(bench.BOOT_STAGE_RE.search("sd: session enter t_ms=640"))
        self.assertIsNotNone(bench.BOOT_STAGE_RE.search("display: started t_ms=42"))

    def test_boot_report_reads_first_paint_from_witnessed_boots(self) -> None:
        cold_run = [
            {"event": "run_start", "suite": "storage-cache", "reset_before": True},
            {"event": "storage_catalog", "action": "load", "elapsed_ms": 30, "t_ms": 1100},
            {"event": "render", "view": "Home", "t_ms": 3200},
            {"event": "render", "view": "Library", "t_ms": 9000},
        ]
        attach_run = [
            {"event": "run_start", "suite": "page-turn", "reset_before": False},
            {"event": "render", "view": "Reading", "t_ms": 500000},
        ]
        paints, stages, warnings = bench.boot_report([cold_run, attach_run])
        self.assertEqual(paints, {"cold": [3200]})
        self.assertEqual(stages, {})
        self.assertEqual(warnings, [])

    def test_boot_report_collects_stages_before_first_paint(self) -> None:
        run = [
            {"event": "run_start", "suite": "sleep-sync", "reset_before": True},
            {"event": "boot_stage", "stage": "main: spawn display", "t_ms": 140},
            {"event": "boot_stage", "stage": "display: started", "t_ms": 150},
            {"event": "render", "view": "Home", "t_ms": 3200},
            # Per-session line after the paint must not count as boot stage.
            {"event": "boot_stage", "stage": "sd: session enter", "t_ms": 9000},
        ]
        paints, stages, _ = bench.boot_report([run])
        self.assertEqual(paints, {"cold": [3200]})
        self.assertEqual(
            stages,
            {"main: spawn display": [140], "display: started": [150]},
        )


class SplitRunsTests(unittest.TestCase):
    def test_markerless_input(self) -> None:
        events = [{"event": "render"}, {"event": "input"}]
        self.assertEqual(bench.split_runs(events), [events])

    def test_multiple_run_start_segments(self) -> None:
        e1 = {"event": "run_start", "id": 1}
        e2 = {"event": "render"}
        e3 = {"event": "run_start", "id": 2}
        e4 = {"event": "input"}
        self.assertEqual(
            bench.split_runs([e1, e2, e3, e4]),
            [[e1, e2], [e3, e4]],
        )

    def test_pre_marker_events(self) -> None:
        e1 = {"event": "render"}
        e2 = {"event": "run_start"}
        e3 = {"event": "input"}
        self.assertEqual(
            bench.split_runs([e1, e2, e3]),
            [[e1], [e2, e3]],
        )

    def test_empty_latest_run(self) -> None:
        e1 = {"event": "run_start"}
        e2 = {"event": "render"}
        e3 = {"event": "run_start"}
        self.assertEqual(
            bench.split_runs([e1, e2, e3]),
            [[e1, e2], [e3]],
        )
        self.assertEqual(bench.split_runs([]), [])



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

    @patch("builtins.print")
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

    @patch("builtins.print")
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


class BenchCaptureLoopTests(unittest.TestCase):
    def test_capture_waits_for_paired_prestage_across_intervening_log(self) -> None:
        """Capture continues past unrelated logs until event=='prestage' arrives."""
        lines = [
            "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 flush_ms=400 t_ms=100\n",
            "[LOG_INF] Unrelated firmware message\n",
            "bench: prestage staged=true elapsed_ms=24 t_ms=124\n",
            "bench: render view=Reading mode=Fast page=2 chapter=1 layout_ms=10 flush_ms=400 t_ms=200\n",
        ]
        written: list[dict] = []

        def event_cb(e: dict) -> None:
            written.append(e)

        counts = bench.process_capture_stream(
            lines,
            "page-turn",
            stop_target=("reading_render", 1),
            print_lines=False,
            event_callback=event_cb,
        )

        self.assertEqual(counts.get("reading_render"), 1)
        self.assertEqual(counts.get("prestage"), 1)
        self.assertEqual(len(written), 2)
        self.assertEqual(written[0]["event"], "render")
        self.assertEqual(written[1]["event"], "prestage")

    def test_capture_stops_immediately_for_structured_combined_render(self) -> None:
        """Structured combined render with prestage_ms stops without waiting for standalone prestage."""
        lines = [
            "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 flush_ms=400 prestage_ms=15 t_ms=100\n",
            "bench: prestage staged=true elapsed_ms=24 t_ms=124\n",
        ]
        written: list[dict] = []

        def event_cb(e: dict) -> None:
            written.append(e)

        counts = bench.process_capture_stream(
            lines,
            "page-turn",
            stop_target=("reading_render", 1),
            print_lines=False,
            event_callback=event_cb,
        )

        self.assertEqual(counts.get("reading_render"), 1)
        self.assertEqual(counts.get("prestage", 0), 0)
        self.assertEqual(len(written), 1)
        self.assertEqual(written[0]["event"], "render")

    def test_capture_bounded_fallback_when_prestage_missing(self) -> None:
        """Capture stops boundedly if prestage telemetry never arrives."""
        lines = [
            "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 flush_ms=400 t_ms=100\n",
        ] + [f"[LOG_INF] Intervening log {i}\n" for i in range(10)]
        written: list[dict] = []

        def event_cb(e: dict) -> None:
            written.append(e)

        counts = bench.process_capture_stream(
            lines,
            "page-turn",
            stop_target=("reading_render", 1),
            print_lines=False,
            event_callback=event_cb,
        )

        self.assertEqual(counts.get("reading_render"), 1)
        self.assertEqual(counts.get("prestage", 0), 0)
        self.assertEqual(len(written), 1)

    def test_capture_silent_device_fallback_when_prestage_missing(self) -> None:
        """Capture stops when deadline expires even if serial stream is completely silent (no newlines)."""
        import time

        deadline_val: list[float] = []

        def silent_lines():
            yield "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 flush_ms=400 t_ms=100\n"
            while True:
                time.sleep(0.01)
                if deadline_val and time.monotonic() >= deadline_val[0]:
                    return

        written: list[dict] = []

        def event_cb(e: dict) -> None:
            written.append(e)

        started = time.monotonic()
        counts = bench.process_capture_stream(
            silent_lines(),
            "page-turn",
            stop_target=("reading_render", 1),
            print_lines=False,
            event_callback=event_cb,
            pending_prestage_timeout_s=0.05,
            on_deadline_set=lambda d: deadline_val.append(d),
        )
        elapsed = time.monotonic() - started

        self.assertEqual(counts.get("reading_render"), 1)
        self.assertEqual(counts.get("prestage", 0), 0)
        self.assertEqual(len(written), 1)
        self.assertLess(elapsed, 1.0)


if __name__ == "__main__":
    unittest.main()
