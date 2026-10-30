# Wolf2 Tray

A small Windows tray program for DCS pilots on [wolf2.net](https://wolf2.net). While a
squadron recording is live, it sends **your own aircraft's** telemetry from your PC to the
site, so the debrief can show what the server never sees: your exact flight data (AOA,
IAS, Mach, G), and — with the optional DCS hook — where you were looking and what your
hands were doing.

It is open source so that you can read exactly what it does before you run it.

## What it does, in plain words

**It reads** one thing: the Tacview exporter's real-time telemetry port on your own PC
(`localhost:42674`). Tacview's DCS exporter serves this by default; nothing to configure.

**It keeps** only the object that is your own aircraft. Other players' aircraft are read
off the socket and dropped on the spot. They are never written to disk and never sent.

**It sends** your aircraft's lines, one gzipped file per minute of mission time, to
`https://wolf2.net/api/ownship/…`, signed in as you with Discord, and **only while the
site says a recording is live**. Nothing is sent at any other time. Minutes recorded
while no recording is live wait on disk for up to 24 hours in case one starts, then are
deleted. A minute only goes to a recording that started within the two hours before it
was recorded; the tray asks the site which recordings are live only while it has minutes
waiting, and never while idle.

**It stores** three things on your PC:

| What | Where |
| --- | --- |
| Your Discord sign-in | Windows Credential Manager, `net.wolf2.desktop` / `discord-session` (shared with Wolf2 Desktop, so you sign in once) |
| A log (no tokens, no telemetry bodies) | `%LOCALAPPDATA%\Wolf2 Tray\tray.log.YYYY-MM-DD`, one file per day |
| Minutes waiting to be sent | `%LOCALAPPDATA%\Wolf2 Tray\spool\` |

It holds no API keys, no bucket credentials and no secrets of the site's. The Discord
client id it uses is the site's public one, the same one every browser sign-in carries.

## Install

1. Download `wolf2-tray.exe` from the [latest release](https://github.com/fastjetperformance/wolf2-tray/releases).
2. Run it. An icon appears in the tray (the red ring). Right-click → **Sign in with
   Discord**, approve in the browser, close the tab.
3. Fly. The status line in the menu goes *Waiting for DCS* → *Connected, no aircraft* →
   *Pushing <your name> to <recording>*.

Tick **Start with Windows** if you want it there every time. **Quit** stops it entirely.

### Requirements

- Windows 10 or 11.
- DCS World with Tacview's exporter installed (the normal Tacview install does this).
  Real-time telemetry is on by default: DCS → Options → Special → Tacview, port 42674.
- Membership of the squadron's Discord. The site decides what you may push to.
- Tacview Advanced is what makes the exporter include flight data (AOA, IAS, Mach, G…)
  for your own aircraft. Without it the tray can still find and push your aircraft's
  position, which adds nothing the server lacks.

### Settings (optional)

`%LOCALAPPDATA%\Wolf2 Tray\config.toml` — every key optional:

```toml
# base_url = "https://wolf2.net"
# tenant = "shadowlands"     # default: the first tenant your membership lists
# port = 42674               # the exporter's real-time telemetry port
# pilot_name = "Airboss"     # pick your aircraft by its Pilot label instead of by flight data
```

## Verifying a release

Every release is built by GitHub Actions on a clean runner from a tagged commit, and the
build's provenance is attested. With the [GitHub CLI](https://cli.github.com/):

```
gh attestation verify wolf2-tray.exe --owner fastjetperformance
```

The release notes carry the exe's SHA-256; `certutil -hashfile wolf2-tray.exe SHA256` on
Windows prints yours. `Cargo.lock` is committed, so the exact dependency set is in the
repository, and the About item in the menu shows the version and commit the binary was
built from.

Building it yourself: install [Rust](https://rustup.rs/), then `cargo build --release`.
The exe is `target/release/wolf2-tray.exe`.

## Privacy

- Only your own aircraft leaves your PC. Other players' data is read and dropped.
- Nothing is sent unless a recording is live on the site.
- What is sent is attributed to you and visible to the squadron's members in the
  recording's debrief, under the same retention as the recording. Your telemetry is
  deleted with the rest of your data if you ask the site to erase you.
- The tray never records audio, screenshots, keystrokes, or anything from other programs.

## How it fits together

```
DCS ──► Tacview exporter ──► localhost:42674 ──► wolf2-tray ──► wolf2.net /api/ownship
                                                   │
                                                   └─ spool (24 h) when no recording is live
```

`src/exporter.rs` speaks Tacview's real-time telemetry protocol. `src/ownship.rs` picks
your aircraft. `src/segment.rs` writes the minute files. `src/spool.rs` and
`src/delivery.rs` hold them and push them. `src/auth.rs` is the Discord sign-in, a copy
of Wolf2 Desktop's, kept in step with it.

## License

MIT — see `LICENSE`.
