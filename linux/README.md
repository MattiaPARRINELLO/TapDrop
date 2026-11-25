# TapDrop for Linux

The Linux half of [TapDrop](../../README.md): the phone sends, the computer receives, with no cable,
on the local network or through a server of your own.

This directory holds the "computer" side of the project, written in Rust. The existing Android app
works unchanged: it speaks exactly the protocol described in the repository's
[`PROTOCOL.md`](../../PROTOCOL.md).

## What it does

| Feature | Detail |
|---|---|
| Pairing | six-digit code, P-256 ECDH, HKDF-SHA256, approved on the computer |
| Sessions | AES-256-GCM, implicit counters, replays and reordering refused |
| Receiving | files, photos, videos, text, one-time codes, clipboard images |
| Clipboard | both ways, text, image or copied files, through `wl-clipboard` |
| Links | opened in Brave, raised to the front, with broken URLs cleaned up |
| OCR | reads the text of screenshots with `tesseract` |
| Outbox | `tapdrop-linux send`, announced on the phone's live link |
| Discovery | DNS-SD `_tapdrop._tcp` through avahi, or a built-in mDNS responder |
| Tap | the phone against the camera bar sends what is on its screen |
| Island | animated pill at the top of the screen: detection, progress, result, codes, hints |
| Status bar | StatusNotifierItem icon with a menu, pending pairings, quit |
| Window | state, phones, pairing, settings (`tapdrop-linux ui`) |

## Requirements

Arch:

```sh
sudo pacman -S --needed rust gtk4 gtk4-layer-shell wl-clipboard avahi tesseract tesseract-data-eng \
    libnotify socat
```

Ubuntu 24.04:

```sh
sudo apt install build-essential libgtk-4-dev libgtk4-layer-shell-dev wl-clipboard avahi-daemon \
    tesseract-ocr libnotify-bin socat
```

`tesseract`, `avahi` and `gtk4-layer-shell` are optional: without them, OCR falls back to the image
clipboard, discovery uses the built-in responder, and the island cannot be shown.

## Build and run

```sh
cargo build --release
./target/release/tapdrop-linux run
```

The service listens on port **47913** (a free port if that one is taken) and advertises itself under
the machine's name. On the phone: TapDrop, **Pair with your Mac**, pick the computer, compare the
code, then approve it:

```sh
tapdrop-linux pending
tapdrop-linux approve 123456
```

## User service

The service runs without a session open and restarts itself. Install the binary in your `PATH`, then
the unit:

```sh
install -Dm755 target/release/tapdrop-linux ~/.local/bin/tapdrop-linux
mkdir -p ~/.config/systemd/user
cp scripts/tapdrop.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tapdrop
journalctl --user -u tapdrop -f
```

## Commands

```
tapdrop-linux run [--auto-pair]   start the service (with its icon)
tapdrop-linux ui                  open the window
tapdrop-linux island              show the island alone (debugging)
tapdrop-linux status              state, devices, reachable addresses
tapdrop-linux sensors             what the light and proximity sensors see
tapdrop-linux send <file…>        queue files for the phone
tapdrop-linux send --clipboard    send this computer's clipboard
tapdrop-linux devices             paired phones
tapdrop-linux forget <id>         forget a phone
tapdrop-linux rename <name>       rename this computer for the phone
tapdrop-linux folder <path>       change where received files land
tapdrop-linux pending | approve <code> | deny <code>
tapdrop-linux quit                stop the service
```

## Files and sockets

| What | Where |
|---|---|
| state, devices, outbox | `~/.config/tapdrop-linux/state.toml` |
| control socket | `$XDG_RUNTIME_DIR/tapdrop-linux/control.sock` |
| island socket | `$XDG_RUNTIME_DIR/tapdrop-linux/island.sock` |
| received files | `~/Downloads` (the XDG folder) by default |

## How a tap is judged

Three proofs, in this order:

1. **proximity**: something lands on the camera bar (20 Hz, tens of milliseconds);
2. **light**: the phone says its screen is white, the camera must see the bar get much brighter;
3. **dark echo**: the phone says its screen is black, the camera must see the bar darker than the room.

A hand can produce 1 and 3, never 2. A lamp can produce 2, never 3. On this machine a full tap takes
240 to 400 ms, and the light threshold learns itself from successful taps.

The chassis light sensor reports once a second, so it is only the fallback when the webcam is busy.
The camera runs with a fixed exposure, because auto-exposure would pull a white screen back to
mid-grey.

## Away from the local network

TapDrop goes through no server by default. When the two devices cannot reach each other (a school
Wi-Fi with client isolation, a phone on cellular), four options, from simplest to most flexible:

1. **USB**: `adb reverse tcp:47913 tcp:47913`, then `127.0.0.1:47913` in TapDrop → *Edit address*.
2. **The phone shares its connection** (hotspot or Bluetooth tethering): the computer joins the
   phone's network, discovery works again, nothing to type.
3. **An SSH relay through your own server**: the computer keeps an outgoing connection and publishes
   the port, the phone connects to the server's address. See `scripts/relay-ssh.sh` and
   `scripts/tapdrop-tunnel.service`. Open TCP 47913 in your cloud's security list. No app to install
   on the phone.
4. **A WireGuard hub on your own server**: `scripts/wireguard-hub.sh` and `scripts/wireguard-peer.sh`
   produce the configurations and a QR code for the phone's WireGuard app.

## Design

The island follows the project's [`docs/DESIGN.md`](../docs/DESIGN.md) rather than improvising: night
`#08080A`, bone text `#F4F3EF`, secondary `#A2A2A8`, the light glow `#D6DEFF`, no accent hue, a notch
shape (flat top, rounded bottom), the progress ring as the leading visual, an ink check on a bone disc
for success, and the system's springs (island 0.42 s / 0.78, content 0.38 s / 0.86).

The surface ignores the panel's exclusive zone (`set_exclusive_zone(-1)`), so it starts at the physical
edge of the screen like a real notch instead of being pushed under the top bar.

When a file arrives, its preview is decoded off the drawing thread (a phone photo would hitch the
animation), drawn at 34 px with rounded corners, and two quiet buttons appear on the right: open the
file, or its folder. The result stays eight seconds, and the countdown pauses while the pointer is on
the pill.

The project's two fonts are embedded in the binary and registered with the process at startup, without
touching the person's font directory: `fonts/Geist[wght].ttf` and `fonts/GeistMono[wght].ttf`, the same
OFL files as `fonts/` at the repository root.

## Architecture

| Crate | Module | Role |
|---|---|---|
| `tapdrop-protocol` | `wire` | length-prefixed frames, AES-256-GCM channel with counters |
| | `crypto` | P-256 ECDH, HKDF-SHA256, the pairing code |
| | `files` | safe names, partial files, never overwriting |
| | `lightkey` | one round's state machine: proximity, light, dark echo |
| | `sensors` | what the sensors report (`Lux`, `Proximity`) |
| `tapdrop-host` | `server` | TCP listener, pairing, sessions, receiving, sending |
| | `watch` | live link, presence, capabilities, renaming |
| | `tap` | ties sensors, camera and live links together |
| | `iio` | the light and proximity sensors |
| | `camera` | the webcam as a light sensor, thirty frames a second |
| | `island` | the animated pill: notch shape, ring, check, springs, event socket |
| | `tray` | StatusNotifierItem icon and its menu |
| | `ui` | GTK4 window |
| | `mdns` | DNS-SD discovery (avahi or the built-in responder) |
| | `net` | reachable addresses (Wi-Fi, WireGuard, Bluetooth) |
| | `store` | persisted state, devices, outbox, names |
| | `control` | control socket and a blocking client for GTK |

## Known limits

- **Samsung Internet** only exposes the host in its accessibility tree, never the path. A tap from
  Samsung therefore opens the site, not the exact page. Chrome exposes the full URL. Sharing from
  Samsung sends the exact URL.
- **Infrared camera** is useless here: a screen emits no infrared, so the IR camera would only see its
  own illuminator, which makes it behave like a presence sensor.
- **Webcam LED** lights up during a tap, for about a quarter of a second.
- Raising the browser after a link needs `kdotool` on KDE Wayland; without it, KWin decides where the
  focus goes.
- **Real blur** is not possible yet: the maintained KWin effect stops at Plasma 6.4, and this release
  is 6.8.
- The official APK still says "Mac": see the Android patch in this repository.

## Debugging

The service stays quiet, save for the outcome of each tap and errors. `TAPDROP_DEBUG=1` turns on the
full trace: every light reading during a round, every batch received, every event sent to the island.

```sh
systemctl --user set-environment TAPDROP_DEBUG=1
systemctl --user restart tapdrop
journalctl --user -u tapdrop -f
```

## Troubleshooting

```sh
tapdrop-linux sensors                 # light and proximity in real time
journalctl --user -u tapdrop -n 50    # the service's journal
systemctl --user status tapdrop
tapdrop-linux status                  # addresses to type on the phone
```

A refused tap shows the reason in the island (`Turn your phone around: its screen goes against the
camera`, `Press the screen flat on the camera bar`, `Keep your phone on the camera bar a moment
longer`). The journal gives the exact reason of the round: `facing`, `not-seen`, `stayed-lit`, `busy`,
`cooldown`.

## License

MIT, like TapDrop. The protocol and the original apps are by
[0x1p0](https://github.com/0x1p0/TapDrop).
