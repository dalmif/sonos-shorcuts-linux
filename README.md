# Sonos

A small command-line tool to control a Sonos speaker on your local network:
volume, mute, play/pause and track skipping. It finds the speaker for you and
can bind its commands to global keyboard shortcuts.

## Download

Grab the prebuilt x86_64 Linux binary from
[Releases](https://github.com/dalmif/sonos-shorcuts-linux/releases), extract it
and run `./sonos init`.

## Build and run

Requires [Rust](https://rustup.rs).

```sh
cargo build --release
mkdir -p bin && cp target/release/sonos bin/
./bin/sonos init
```

`init` finds your speaker and saves it to `sonos.json` next to the binary.

## Commands

```
sonos init          find the speaker and save the config
sonos up [N]        volume up
sonos down [N]      volume down
sonos set N         set volume (0-100)
sonos get           show volume
sonos mute          mute / unmute
sonos play-pause    play / pause
sonos next          next track
sonos previous      previous track
sonos discover      list Sonos speakers on the network
sonos refresh       re-find the speaker if its IP changed
sonos uninstall     remove shortcuts and autostart
```

## Keyboard shortcuts

Each command can be bound to a key combination in the `shortcuts` section of
`sonos.json`, for example:

```json
"shortcuts": {
  "up": "<Super><Alt>XF86AudioRaiseVolume",
  "down": "<Super><Alt>XF86AudioLowerVolume"
}
```

Run `sonos refresh` to apply. Shortcuts are registered with GNOME; on other
desktops, bind the commands (e.g. `sonos up`) in your desktop's keyboard settings.

## Configuration

Other options in `sonos.json`: `step` (volume change per press), `max_volume`,
and `autostart` (run `sonos refresh` at login).

## Releasing

Bump the version in `Cargo.toml`, add a `## vX.Y.Z` section to `CHANGELOG.md`,
then push a matching tag (e.g. `git tag v0.2.0 && git push origin v0.2.0`). CI
builds the binary and publishes a GitHub release with that changelog section as
its notes.
