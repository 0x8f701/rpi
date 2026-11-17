"""Argv-only checks for the hub TUI tmux harness. Never executes tmux."""

from __future__ import annotations

import os
import subprocess
import unittest
from unittest import mock

import run_hub_tui_campaign as campaign


class PrivateTmuxSocketTest(unittest.TestCase):
    def test_run_tmux_uses_one_private_socket_and_strips_tmux_env(self) -> None:
        seen: list[list[str]] = []

        def fake_run(argv, **kwargs):
            seen.append(list(argv))
            self.assertEqual(kwargs["env"].get("TMUX"), None)
            self.assertEqual(kwargs["env"].get("TMUX_PANE"), None)
            self.assertNotIn("-S", argv)
            return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

        with mock.patch.dict(os.environ, {"TMUX": "/tmp/main.sock", "TMUX_PANE": "%1"}, clear=False):
            with mock.patch.object(campaign.subprocess, "run", fake_run):
                campaign.run_tmux("display-message", "-p", "-t", "sess:0.0", "#{pane_dead}")
                campaign.run_tmux("kill-session", "-t", "sess", check=False)

        self.assertEqual(len(seen), 2)
        socket = campaign._TMUX_SOCKET
        self.assertTrue(socket.startswith(f"rpi-e2e-hub-{os.getpid()}-"))
        self.assertNotIn("/", socket)
        for argv in seen:
            self.assertEqual(argv[:5], ["tmux", "-L", socket, "-f", "/dev/null"])
        self.assertEqual(seen[0][5:], ["display-message", "-p", "-t", "sess:0.0", "#{pane_dead}"])
        self.assertEqual(seen[1][5:], ["kill-session", "-t", "sess"])
        self.assertNotIn("kill-server", seen[1])

    def test_start_detached_session_respawns_explicit_pane(self) -> None:
        calls: list[tuple[str, ...]] = []

        def fake_run_tmux(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
            calls.append(args)
            return subprocess.CompletedProcess(["tmux", *args], 0, stdout="", stderr="")

        with mock.patch.object(campaign, "run_tmux", fake_run_tmux):
            pane = campaign.start_detached_session("sess", "/work", ["env", "HOME=/work", "rpi"])

        self.assertEqual(pane, "sess:0.0")
        self.assertEqual(calls[0][:6], ("new-session", "-d", "-s", "sess", "-x", "120"))
        self.assertEqual(calls[0][-2:], ("sleep", "86400"))
        self.assertNotIn("respawn-pane", calls[0])
        self.assertEqual(calls[1], ("set-option", "-w", "-t", "sess:0", "remain-on-exit", "on"))
        self.assertEqual(calls[2][:6], ("respawn-pane", "-k", "-t", "sess:0.0", "-c", "/work"))
        self.assertEqual(calls[2][6:], ("env", "HOME=/work", "rpi"))


class SessionExitStatusTest(unittest.TestCase):
    def _run(self, *stdout_values: str) -> int:
        values = iter(stdout_values)

        def fake_run_tmux(*args, **kwargs):
            return subprocess.CompletedProcess(["tmux"], 0, stdout=next(values), stderr="")

        with mock.patch.object(campaign, "run_tmux", fake_run_tmux):
            with mock.patch.object(campaign.time, "sleep", lambda _seconds: None):
                return campaign.wait_for_session_exit("sess", 1.0)

    def test_dead_pane_waits_for_numeric_status_instead_of_defaulting(self) -> None:
        self.assertEqual(self._run("0 ", "1 ", "1 0"), 0)

    def test_dead_pane_with_non_numeric_status_is_contextual_error(self) -> None:
        with self.assertRaisesRegex(AssertionError, "non-numeric pane exit status 'tough'"):
            self._run("1 tough")

    def test_signal_exit_is_never_reported_as_clean(self) -> None:
        with self.assertRaisesRegex(AssertionError, "terminated by signal"):
            self._run("1 -15")


class PressureProviderStateTest(unittest.TestCase):
    def test_terminal_counts_track_actual_batches(self) -> None:
        self.assertEqual(
            campaign.pressure_provider_state(4),
            {"main": 3, "a": 2, "b": 2, "producer": 15, "filler": 256},
        )
        self.assertEqual(campaign.pressure_provider_state(3)["producer"], 12)
        self.assertEqual(campaign.pressure_provider_state(3)["filler"], 192)


class PressureSettledTest(unittest.TestCase):
    @staticmethod
    def _pane(
        batches: int = 4,
        omit: str | None = None,
        replace: tuple[str, str] | None = None,
    ) -> str:
        rows = [
            "• A ⟦A⟧ · completed · 1ms",
            "• B ⟦B⟧ · completed · 1ms",
            "• Producer ⟦Producer⟧ · completed · 1ms",
        ]
        for batch in range(1, batches + 1):
            for index in range(1, campaign.PRESSURE_BATCH_SIZE + 1):
                name = f"Filler-{batch:02d}-{index:02d}"
                if name == omit:
                    continue
                if replace is not None and name == replace[0]:
                    name = replace[1]
                rows.append(f"• {name} ⟦Filler⟧ · completed · 1ms")
        header = f"• Task {3 + batches * campaign.PRESSURE_BATCH_SIZE} agents"
        return "\n".join([header, *rows])

    def test_all_256_filler_rows_completed_pass(self) -> None:
        campaign.assert_pressure_settled([self._pane()], 4)

    def test_missing_filler_row_fails(self) -> None:
        with self.assertRaisesRegex(AssertionError, "Filler-02-37"):
            campaign.assert_pressure_settled([self._pane(omit="Filler-02-37")], 4)

    def test_same_length_wrong_filler_name_fails(self) -> None:
        with self.assertRaisesRegex(AssertionError, "Filler-04-64"):
            campaign.assert_pressure_settled(
                [self._pane(replace=("Filler-04-64", "Filler-04-65"))], 4
            )


if __name__ == "__main__":
    unittest.main()
