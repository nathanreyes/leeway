# Remote access

Keep Leeway and its database on one machine. Connect to that machine with SSH
from your other devices. Each connection uses the same budget and app session.

## Set up the host

1. Install Leeway and tmux on the host. Both commands must be on your SSH user's
   `PATH`.
2. Enable the host's SSH service and set up access with your SSH key. Use the
   same host user for each device so each connection finds the same data and
   tmux session.
3. Check that `ssh your-host` works. Replace `your-host` with your SSH alias or
   `user@hostname`.
4. Connect and start Leeway:

   ```sh
   ssh -t your-host 'tmux new-session -A -s leeway leeway'
   ```

SSH's `-t` allocates a terminal. tmux's `-A` reconnects to the named session when
it exists, or creates it when it does not. See the [SSH manual](https://man.openbsd.org/ssh)
and [tmux guide](https://github.com/tmux/tmux/wiki/Getting-Started).

Press `Ctrl+b`, then `d` to detach and leave Leeway running. Run the same command
to reconnect after detaching or losing the network connection. Press `q` inside
Leeway to quit the app. tmux does not survive a host reboot; run the command again
to start Leeway with the saved budget.

Connections to this session share the screen and input. Use this session when
switching devices. The host must stay on and reachable, and you need a network
connection to use it remotely.

## Move an existing budget

Folder sync is no longer supported. Leeway keeps the existing local database
path and ignores the old `config.json` and `device.json`. It leaves those files,
shared sync folders, and recovery backups untouched. It does not choose or load
a snapshot from an old sync folder.

1. On the device with the budget you want to keep, open Settings → Storage and
   press `b`. Note the backup path in the status message. On an older build
   without this action, quit Leeway and copy its local `leeway.db` instead.
2. On the host, open Leeway once to create its data directory. Open Settings →
   Storage and note the database path, then quit Leeway on the host.
3. Copy the source backup to a temporary path on the host with `scp` or your
   preferred file transfer tool.
4. With every Leeway instance on the host closed, keep a copy of the host's
   current database. Put the transferred backup at the database path with the
   name `leeway.db`.
5. Reconnect with the SSH command above and check your balances and plans.

Use the host's budget from then on. Edits in another machine's local Leeway
install will not appear on the host.

## Back up and restore

In Settings → Storage, press `b` to create a consistent SQLite backup while the
app runs. Backups go into `recovery` beside the database. Older recovery files
remain usable SQLite snapshots. Keep copies off the host and remove old backups
as needed; Leeway does not prune them.

To restore, quit all Leeway instances on the host, save a copy of the current
database, then copy the chosen backup to the database path as `leeway.db`.
Restart Leeway and check the restored budget.
