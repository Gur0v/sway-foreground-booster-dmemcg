# sway-foreground-booster-dmemcg

Raises a Steam game's `dmem.low` while its Sway window is focused and clears it when focus leaves. Under VRAM pressure, this keeps the focused game's memory from being evicted in favor of background apps. It does nothing while there is enough VRAM.

## Requirements

- cgroup v2 with `dmem` exposed (`/sys/fs/cgroup/dmem.capacity` should list your GPU)
- Sway and systemd
- [dmemcg-booster](https://gitlab.steamos.cloud/holo/dmemcg-booster), both the system and user services running. It protects the parent cgroups.
- `dmem` enabled in `user@UID.service` and `app.slice`
- Rust toolchain

If your systemd rejects `dmem` in `Delegate=` (the journal says "Invalid controller name"), enable it yourself before dmemcg-booster's user service starts. Run `systemctl --user edit dmemcg-booster-user.service` and add:

```
[Service]
ExecStartPre=/bin/sh -c 'b=/sys/fs/cgroup/user.slice/user-%U.slice/user@%U.service; echo +dmem > $$b/cgroup.subtree_control; echo +dmem > $$b/app.slice/cgroup.subtree_control'
```

Before expecting any effect, check that `app.slice/dmem.low` shows your GPU's full capacity.

## Install

```
cargo install --path .
cp sway-foreground-booster-dmemcg.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sway-foreground-booster-dmemcg.service
```

Stop it with `systemctl --user stop sway-foreground-booster-dmemcg.service`. Logs: `journalctl -b -t sway-foreground-booster-dmemcg`.

## Games

Set each game's Steam launch options to:

```
gamemoderun systemd-run --user --scope --slice=app.slice -- %command%
```

Restart the game, then check `/proc/<game-pid>/cgroup`. The game and the Steam reaper should share their own `run-p*.scope` under `user@UID.service/app.slice`.

## Notes

Only scopes with a user-writable, all-zero `dmem.low` are touched, and on focus loss only values the booster set are cleared. It does not modify ancestor slices, since dmemcg-booster handles those. If it dies mid-boost, check the game's `dmem.low` before clearing it by hand.

## License

BSD-3-Clause, see [LICENSE](LICENSE).

