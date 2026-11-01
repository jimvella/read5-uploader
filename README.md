# read5-uploader

Contributes air-band audio from your receiver to [Read5](https://read5.net).
It records the channels [rtl_airband](https://github.com/rtl-airband/RTLSDR-Airband)
sends to a local UDP port, splits them into transmissions, encodes them as
Opus and uploads one archive per channel per minute. It runs as a single
program beside rtl_airband, typically on a Raspberry Pi.

The source is public so you can see exactly what it does with your audio
and your connection before you run it.

## What it sends, and to whom

The program makes HTTPS requests to one address, the Read5 ingest route
(`https://read5.net/api/ingest` unless you change `[upload] url`). Every
request carries the upload key Read5 issued for your receiver. It sends:

| Object | Contents | When |
|---|---|---|
| `audio/{channel}/{YYYYMMDD}/{HHMM}-{receiver}.zip` | That minute's transmissions on one channel: the Opus audio as received, a levelled copy (`[process]`), and `manifest.json` with each transmission's start, end and peak level | Each minute with audio on that channel |
| `audio/_up/{YYYYMMDD}/{HHMM}-{receiver}.zip` | `manifest.json`: the minute, the channels that had audio, and any whose rtl_airband stream was down | Every minute the program runs |
| `receivers/{receiver}.json` | The receiver document: `[receiver]` name and, if you set them, `lat`, `lon` and `location`; the program version; start and stop times; the `[[channels]]` list; the audio format | At start, when the sample rate is first seen, and at a clean stop |

Read5 publishes this. That includes the receiver document, so set `lat`
and `lon` only if you're happy for your receiver's position to be public.

It sends nothing else: no system details, no other files, nothing from
rtl_airband beyond the audio of the channels in `[[channels]]`. It
receives on the UDP ports in your rtl_airband config, on the address in
`[rtl_airband] listen` (`127.0.0.1` by default), and listens on no other
port. The only thing it runs is `timedatectl show -p NTPSynchronized` once a
minute, to hold uploads while the clock is wrong.

The key lets the receiver write only its own objects, and channel archives
only for the channels it was issued. It can't read or change anyone else's
uploads. Keep it in `/etc/read5/read5.env`, which only root can read.
If it leaks, ask Read5 to disable it and issue a new one.

## Install on a Raspberry Pi

You need rtl_airband already working, and an upload key and channel ids
from Read5.

```
tar xzf read5-uploader-*-arm64.tar.gz      # armhf on 32-bit Pi OS
cd read5-uploader-*/
sudo ./install.sh
```

Then:

1. Add one output per channel in `rtl_airband.conf`, next to whatever is
   already there, with a different port for each channel:

   ```
   { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6101; continuous = true; }
   ```

   `continuous = true` is required. Channels in scan mode and mixers can't
   be recorded (`read5-uploader streams` says why). Restart rtl_airband.
2. Put the key in `/etc/read5/read5.env`:
   `READ5_UPLOAD_KEY=r5_<receiver>.<secret>`
3. In `/etc/read5/read5-uploader.toml`, add a `[[channels]]` entry for each
   frequency, with the channel id Read5 issued, and give the receiver a name.
4. Check everything, including that Read5 accepts the key:

   ```
   sudo sh -c 'set -a; . /etc/read5/read5.env; /opt/read5/read5-uploader check'
   ```

5. `sudo systemctl start read5-uploader`

To upgrade, run `install.sh` from a newer bundle. It keeps `/etc/read5`.

## Running

| | |
|---|---|
| Logs | `journalctl -u read5-uploader -f` |
| Status | `/var/lib/read5/status.json`: uploads waiting, and per stream whether it's up, its clock drift and jitter, and lost batches |
| Spool | `/var/lib/read5/spool`: audio waiting to be sealed or uploaded |
| Restart | `sudo systemctl restart read5-uploader` (needed after editing the config) |

The service restarts 5 s after any exit, and systemd kills and restarts it
if it hangs for 60 s. It runs separately from rtl_airband, so either can
restart without the other. While rtl_airband is stopped, its channels show
as down in the status file and in the minute markers. While the network or
Read5 is unreachable, recording carries on and minutes wait in the spool,
up to `max_backlog_minutes` (three days by default). Every archive is
checked against the MD5 the server reports before it is deleted locally.

## Configuration

[`deploy/read5-uploader.toml`](deploy/read5-uploader.toml) is the installed
example, with every section explained. Environment variables override it:
`READ5_UPLOAD_KEY`, `READ5_UPLOAD_URL`, `READ5_RECEIVER_ID`.

## Building

```
cargo build --release          # needs cmake: libopus is built from source
cargo test
```

The Pi bundles are cross-compiled from Linux with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild):
`bash scripts/package.sh` writes them, with SHA-256 sums, to `dist/`.
After changing dependencies, regenerate the licence list with
`cargo about generate about.hbs -o THIRD_PARTY_LICENSES.md`
([cargo-about](https://github.com/EmbarkStudios/cargo-about)).

## Licence

MIT, see [LICENSE](LICENSE). The binary includes libopus (BSD-3-Clause)
and Rust crates under their own permissive licences, listed in
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md). The rtl_airband
example configs in `tests/fixtures/` are GPL-3.0 test inputs, not part of
the program.
