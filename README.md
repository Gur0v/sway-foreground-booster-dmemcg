# Sway dmem foreground booster

Automatically boosts isolated Steam games' `dmem.low` while their Sway window is focused. BSD-3-Clause.

Requires cgroup v2, Sway, and `dmem` enabled below the user manager's `app.slice`. Set each game's Steam launch options to `gamemoderun systemd-run --user --scope --slice=app.slice -- %command%`, then restart the game. Check `/proc/<game-pid>/cgroup`: the game and Steam reaper must share a new `run-p*.scope` beneath `user@UID.service/app.slice`, separate from Sway and Steam. The scope's `dmem.low` must be user-writable and initially zero. Shared `session-*.scope` is never eligible.

Run from Sway with `cargo run --offline`, or build with `cargo build --offline` and install the included user unit in `~/.config/systemd/user/`, then `systemctl --user daemon-reload && systemctl --user enable --now sway-foreground-booster-dmemcg.service`. Its `ExecStart` points to this checkout's debug binary; rebuild after source changes. Stop with `systemctl --user stop sway-foreground-booster-dmemcg.service` (or Ctrl+C for manual runs).

On focus loss the booster clears regions still matching its boost, leaving externally changed values alone. It rejects unrelated windows, preexisting nonzero limits, unverified scopes, and missing controllers. Sway IPC reconnects. After forced exit or failed cleanup, inspect the game's `dmem.low` before manually clearing stale values. Existing SteamOS dmemcg boosters protect broad ancestors; this program does not modify them or guarantee priority against equally protected siblings. `dmem` availability may change after user-manager restarts.
