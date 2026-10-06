# sway-foreground-booster-dmemcg

Raises a Steam game's `dmem.low` while its Sway window is focused and clears it when focus leaves. BSD-3-Clause.

## Requirements

- cgroup v2 with `dmem` exposed (`/sys/fs/cgroup/dmem.capacity` should list your GPU)
- `dmem` enabled below the user manager's `app.slice`
- Sway and systemd
- Rust toolchain

## Install

```
cargo install --path .
cp sway-foreground-booster-dmemcg.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sway-foreground-booster-dmemcg.service
```

Stop it with `systemctl --user stop sway-foreground-booster-dmemcg.service`. Logs are in `journalctl --user -u sway-foreground-booster-dmemcg`.

## Games

Set each game's Steam launch options to:

```
gamemoderun systemd-run --user --scope --slice=app.slice -- %command%
```

Restart the game, then check `/proc/<game-pid>/cgroup`. The game and the Steam reaper should share their own `run-p*.scope` under `user@UID.service/app.slice`.

## Notes

Only scopes with a user-writable, all-zero `dmem.low` are touched, and on focus loss only values the booster set are cleared. It does not modify ancestor slices, so it won't win against equally protected siblings. If it dies mid-boost, check the game's `dmem.low` before clearing it by hand.
