# How TapDrop was ported to Linux

This document records what was done, measured and decided to make the "computer" half of TapDrop run
on a Linux PC with the stock Android app. It is a technical journal: the numbers come from real
measurements on the machine, not estimates.

## 1. Starting point

TapDrop is MIT-licensed, with a protocol documented in `PROTOCOL.md`: P-256 pairing with a six-digit
code, AES-256-GCM sessions, batches of files and text, a live link, and a proof of physical contact
called the *light key*. The Mac app is Swift, the Android app Kotlin. Nothing in the protocol is
macOS-specific, so a Linux host can stand in for the Mac without touching the phone.

The Linux host is written in Rust, in `linux/`: a workspace with `tapdrop-protocol` (the protocol,
UI-free and hardware-free) and `tapdrop-host` (a binary with the sensors, the island, a status bar
icon and a GTK window).

## 2. The machine and its sensors

Machine: Lenovo Yoga 7 2-in-1 16ILL10, Garuda Linux, KDE Wayland, 120 Hz display, scale factor 1.65.

The camera module (Luxvisions, USB) contains a HID sensor hub exposing three IIO devices:

| Sensor | Path | What it gives |
|---|---|---|
| ambient light | `iio:device0` (`als`) | lux, scale 0.1 |
| proximity | `iio:device1` | one near/far bit, which did not react to a phone |
| distance | `iio:device2` | millimetres, 20 Hz |

### Light measurements

| Situation | Ambient light sensor | Webcam (mean 0-255) |
|---|---|---|
| lit room | ~400 lux | 48 |
| phone's white screen against it | 1 057 lux | 173 |
| black screen against it | 15 to 44 lux | 2.5 |

Contrast is excellent in both cases. But the ambient light sensor **only reports once a second**:
value changes are 1 009 to 1 015 ms apart, and plateaus last multiples of 1 s. Forcing
`sampling_frequency` to 100 Hz is accepted by the driver but the hub ignores it. The camera reads
**thirty frames a second**, and a switch shows in a single frame.

### Proximity measurements

Putting the phone down pushes the distance to `10000` (no return) and the binary channel from 1 to 0,
within 40 to 100 ms. A hand does exactly the same: proximity therefore **cannot** prove it is a screen,
it can only open the question.

## 3. The protocol

Reimplemented to the letter: `u32` big-endian frames, JSON before encryption, `AES-256-GCM` with a
`direction || counter` nonce, P-256 ECDH, HKDF-SHA256, and the pairing code as `u32 % 1e6`.

The tests prove: HKDF matches RFC 5869 test vector 1, both sides derive the same secret, the session
key depends on both nonces, a replay is refused (the test seals a frame once and feeds it twice), and a
secure channel round-trips over a duplex.

On the functional side: pairing, the `ping`, `send`, `getclip`, `inbox`, `watch`, `names`, `unpair`
requests, and the encrypted `forgotten` message for a phone that was forgotten.

## 4. The tap

The protocol expects a "light key": the phone lights its screen white, then black, and the host must
see the light and then the darkness. The Mac version uses the notch sensor, which samples fast. On
this PC, at one reading a second, the protocol's 450 ms windows are impossible.

Decision: **proximity triggers, light proves**.

1. proximity sees the contact, the round starts within tens of milliseconds;
2. the camera looks for the white screen, against 45 % of the learned contact brightness;
3. the black screen must make the reading fall, and the bar must be darker than the room.

A full round takes 240 to 280 ms, and the threshold learns from the median of the last twelve
successful taps (1 042 lux measured here, instead of the 300 default).

## 5. The camera as a light sensor

The 1 Hz ambient sensor kept missing the flashes. The webcam, in the same bump, reads thirty frames a
second: it replaced the sensor for the tap, with three precautions.

- **fixed exposure** (`auto_exposure=1`, time 80): auto-exposure would pull a white screen back to
  mid-grey, which destroys the measurement;
- **exposure restored** after each round, so video calls keep working;
- **the camera only opens during a round**: the visible LED is the cost, about a quarter of a second.

The mean luminance is mapped into the same units as the light sensor (`×8`), which lets the threshold
learning and the dark echo check work unchanged.

The infrared camera was ruled out: a screen emits no infrared, so it would only see its own
illuminator, which makes it behave like proximity.

## 6. The island

The island is a Wayland layer surface (`gtk4-layer-shell`), anchored at the top and centred,
click-through except on the pill. It draws with Cairo and animates with real springs stepped on the
frame clock (120 Hz on this machine).

It follows the project's `docs/DESIGN.md` rather than improvising:

- system palette: opaque night `#08080A`, bone text `#F4F3EF`, secondary `#A2A2A8`, the light glow
  `#D6DEFF`, no accent hue, success in bone rather than green;
- Geist and Geist Mono, embedded in the binary and registered with the process through fontconfig,
  without touching the person's font directory;
- notch geometry: flat top against the screen edge, rounded bottom, 190 × 32 at rest;
- their island's anatomy: a leading visual (progress ring, thumbnail, or glyph), a 15 pt semibold
  title, a 12 pt detail, a monospaced percentage on the right;
- progress ring with a 15 % track, a bone arc, a light dot at its head, and an indeterminate quarter
  turn when the total size is unknown;
- success: a bone disc with an ink check that draws itself, or the preview of the file that just
  landed, decoded off the drawing thread;
- two quiet buttons in the result: open the file, or its folder; the result stays eight seconds, and
  the countdown pauses while the pointer is on the pill;
- the system's springs: island 0.42 s / 0.78, content 0.38 s / 0.86;
- respects "reduce motion" when GTK asks for it;
- Pango layouts cached per state, so nothing is rebuilt at 120 frames a second.

Two bugs were found and fixed along the way, both visible in use:

1. the surface kept its last frame when the island hid, because the compositor clears nothing: the
   window is now **unmapped** when there is nothing to show;
2. events were sent from different sockets and could arrive out of order, a "receiving" after a
   "saved" leaving the card stuck: a single socket guarantees the order.

## 7. Status bar and window

The icon is a StatusNotifierItem (`ksni`) with a menu: open, send the clipboard, accept or refuse a
pairing, quit. The window is GTK4 and talks to the service through the control socket, so it can be
closed without stopping the service.

The service runs under `systemd --user`, with `Restart=always`, and starts with the session.

## 8. The network, and school Wi-Fi isolation

On a normal network, DNS-SD discovery is enough. On the school Wi-Fi, client isolation prevents two
devices from reaching each other, even when they see each other over mDNS. Four workarounds were put
in place, in order of simplicity:

1. **USB**: `adb reverse tcp:47913 tcp:47913`, then `127.0.0.1:47913` in the app;
2. **connection sharing** from the phone, where discovery works again;
3. **SSH relay**: the computer opens an outgoing connection to the server and publishes the port,
   `socat` exposes it; the phone connects to the server's address, with nothing to install;
4. **WireGuard hub** on the server, with a private `10.66.0.0/24` subnet and a QR code for the phone.

The SSH relay was kept: no app to install on the phone, and the content stays end-to-end encrypted,
the server only ever sees TapDrop's ciphertext.

## 9. Decisions and trade-offs

| Decision | Reason |
|---|---|
| proximity as the trigger | the only fast, reliable source at contact |
| light as the proof | a hand cannot light up a screen |
| camera instead of the light sensor | 30 Hz instead of 1 Hz |
| fixed exposure | auto-exposure cancels the contrast |
| automatic pairing off on a public port | the relay's port is exposed |
| a single socket to the island | the order of events matters |
| surface unmapped when idle | no ghost frame is possible |
| width measured on the content | no truncation, a truer motion |

## 10. What remains

- explicit calibration (three holds, median, Reset), which the protocol describes;
- porting the project's own checks (`keyCheck`, `otpCheck`, `timeCheck`) to the Rust side;
- a Linux release artifact in the release workflow;
- real glass blur, blocked for now: the maintained KWin effect stops at Plasma 6.4, this release is
  6.8;
- an APK with "PC" wording and cleaned URLs on the phone side.

## 11. Numbers worth keeping

```
light sensor : ~1 reading / s, room 400 lx, white screen 1057 lx, black screen 15-44 lx
webcam       : 30 frames / s, room 48, white screen 173, black screen 2.5, exposure 80
proximity    : 20 Hz, reaction 40-100 ms, 10000 = no return
full tap     : 240-280 ms
learned level: 1 042 lux (0.45 × contact level)
display      : 120 Hz, scale 1.65
```
