# shouji

A terminal launcher: list your programs in a YAML file and start them all at once, each in its own pane.
It also works as a simple terminal multiplexer (splits, windows, scrollback).

![shouji demo](shouji_htw.gif)

## Install

Linux (x86_64 / aarch64), from GitHub Releases:

```sh
curl -fsSL https://raw.githubusercontent.com/arata/shouji/master/install.sh | sh
```

It installs to `/usr/local/bin` (using `sudo` if needed). Options:

```sh
curl -fsSL .../install.sh | SHOUJI_VERSION=v0.1.0 sh                # a specific version
curl -fsSL .../install.sh | SHOUJI_INSTALL_DIR="$HOME/.local/bin" sh  # another place, no sudo
```

Or build it yourself: `cargo install --path .`

To make a release, push a tag: `git tag v0.1.0 && git push origin v0.1.0`.

## Usage

```sh
cargo run -- -t sample_config/task.yaml   # start with a task opened
cargo run -- -c sample_config/shouji.yaml # use a specific config (pick a task with C-q Space)
cargo run                                 # looks for ./shouji.yaml, then ~/.config/shouji/config.yaml
```

### Sessions (detach / attach)

shouji keeps running in the background, like tmux. Leave with `C-q d` (or the `Leave` button)
and choose **Detach**: your programs keep running even if you close the terminal.

```sh
shouji -t task.yaml        # starts session "task" (named after the file), or attaches if it is running
shouji -s dev -t task.yaml # pick the session name yourself
shouji attach [SESSION]    # go back (also: shouji a)
shouji ls                  # list running sessions
shouji kill [SESSION]      # stop a session and all its programs
```

To start only part of a task, list window or program names after the task file:

```sh
cargo run -- -t sample_config/task.yaml app           # every program in the "app" window
cargo run -- -t sample_config/task.yaml build server  # only "build" and "server"
```

`after` dependencies on programs that were not selected are ignored.

## Configuration

`shouji.yaml`: the tasks listed in the launcher, and settings shared by all of them

```yaml
env:                                 # added to every task below (pre_command works too)
  NETWORK_IF: ${NETWORK_IF:-eth0}
  ROS_IP: $(ip -4 -o addr show $NETWORK_IF | awk '{print $4}' | cut -d/ -f1)
tasks:
  - task.yaml
```

A task opened with `-t` uses the `shouji.yaml` in the same directory as the task file.

`task.yaml`: the programs started together

```yaml
name: dev
env: { APP_ENV: development }       # shouji.yaml < task < window < program (the narrower one wins)
windows:
  - name: app
    programs:
      - name: build
        command: cargo build
      - name: server
        command: cargo run
        after: [{ build: done }]     # wait until build exits successfully (or: started)
        restart: on-failure          # or { when, delay, max }
        log: logs/server.log         # relative to cwd (or where shouji was started)
        stop: docker compose down    # run when the pane is closed or the session ends (or with the Stop button / C-q s)
  - name: tools
    programs:
      - name: git-log
        command: git log --oneline
        autostart: false             # type the command into a shell and wait for Enter
```

`env` values are expanded like a shell string in double quotes: `${VAR:-default}`, `$(command)`, `$HOME/bin:$PATH`.
They are set in order (shouji.yaml, task, window, program, each top to bottom), so a value can use the ones before it.
Write `\$` or `\"` for a literal `$` or `"`. If a value that uses `$` comes out empty, the pane shows a warning.

`pre_command` runs in the same shell before `command` (and before the shell opened by `autostart: false` or `s`).
Like `env`, it can be set on the task, a window or a program; the narrowest one wins.

```yaml
name: ros
pre_command: . /opt/ros/noetic/setup.bash
programs:
  - name: roscore
    command: roscore
```

`if_env` starts a program only when environment variables have the given values; otherwise no pane is made.
`commands` picks one command: the first item whose `if_env` matches (an item without `if_env` is the fallback).

```yaml
env:
  VIEWER: ${VIEWER:-0}
  PLANNER: ${PLANNER:-normal}
programs:
  - name: viewer
    if_env: { VIEWER: 1 }            # every variable listed must match
    command: ./viewer
  - name: planner
    commands:
      - if_env: { PLANNER: [fast, "quick*"] }   # any value in the list; * and ? are wildcards
        command: ./planner --fast
        stop: ./planner --shutdown   # optional: replaces the program's stop when this item is picked
      - command: ./planner           # fallback; without it, no match means no pane
```

The conditions are checked once, when the task is launched, against the environment after `env` is expanded
(so `VIEWER=1 shouji -t task.yaml` works, and so do the defaults). An unset variable counts as empty:
`""` matches empty, `"?*"` matches anything else. `pre_command` is not run for the check.
A program left out is treated like one not selected: an `after` that waits for it is dropped.

Every option is described in `sample_config/task.yaml`. `sample_config/ros.yaml` is a ROS example,
`sample_config/if_env.yaml` an `if_env` / `commands` example.

## Keys (prefix: `Ctrl-q`)

What you type always goes to the selected pane. Most things can be done with the mouse
(tabs, the layout panel, the buttons at the bottom and in stopped panes); keyboard commands
start with `Ctrl-q`: press `Ctrl-q`, let go, then press the key.

| Key | Action |
|---|---|
| `C-q ?` | Help |
| `C-q Space` | Task launcher (`Enter` starts everything, `→` picks windows/programs) |
| `C-q o`, `C-q Tab`, `C-q arrows`, `C-q h/j/k/l` | Move between panes (or click a pane) |
| `C-q z` | Zoom the pane in / out |
| `C-q %` / `C-q "` | Split left/right, top/bottom |
| `C-q q` | Close the pane |
| `C-q s` | Run the program's `stop` command without closing the pane (also the `Stop` button) |
| `C-q c` / `C-q n` / `C-q p` / `C-q 1-9` | New window / switch windows (or click a tab) |
| `C-q [` | Scroll back (`k/j u/d g/G`, `q` to stop; the mouse wheel works too) |
| `C-q t` | Show / hide the layout panel |
| `C-q m` | Mouse on / off |
| `C-q d` | Leave: detach (programs keep running) or quit |
| `C-q C-q` | Send `Ctrl-q` to the program |

In a pane whose program has stopped, press `r` to run it again, `s` for a shell, or `q` to close it.
In a pane waiting to start, press `Enter` to start it now.
Each pane title shows the program's state (`Running`, `Waiting`, `Stopped`, `Finished`, `Shell`).
While the mouse is on, select text with `Shift`+drag. Colors need a terminal with 24-bit color.
