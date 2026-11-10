# read5-uploader

Contributes air-band audio from your receiver to [Read5](https://read5.net).
It records the channels [rtl_airband](https://github.com/rtl-airband/RTLSDR-Airband)
sends to a local UDP port, splits them into transmissions, encodes them as
Opus and uploads one archive per channel per minute. It runs as a single
program beside rtl_airband, typically on a Raspberry Pi, installed either
as a service or with Docker Compose.

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
port. It runs no other program. Once a minute it asks the kernel whether
the clock is NTP-synchronised (what `timedatectl` shows), to hold uploads
while the clock is wrong.

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

## Install with Docker Compose

`ghcr.io/jimvella/rtlsdrairband-read5` is the
[docker-rtlsdrairband](https://github.com/sdr-enthusiasts/docker-rtlsdrairband)
image with this program added as one more service
([`docker/rtlsdrairband/Dockerfile`](docker/rtlsdrairband/Dockerfile)), and
with rtl_airband rebuilt for 16 kHz audio. One container receives the audio
and uploads it. If you already run docker-rtlsdrairband, it takes that
image's place and everything you have set up keeps working; without an
upload key it behaves exactly as docker-rtlsdrairband does.

It is published for 64-bit Raspberry Pi OS (arm64). On another
architecture, or to build it yourself from this source, give Compose the
repository in place of the `image:` line below:

```yaml
    build:
      context: https://github.com/jimvella/read5-uploader.git#main
      dockerfile: docker/rtlsdrairband/Dockerfile
```

`docker compose build` then compiles the uploader and rtl_airband on your
machine: about ten minutes on a Pi 4, once. Run it again to upgrade.

You need Docker with the Compose plugin, an RTL-SDR dongle, and an upload
key and channel ids from Read5. The host must not load the DVB driver for
the dongle (`echo blacklist dvb_usb_rtl28xxu | sudo tee
/etc/modprobe.d/blacklist-rtl.conf`, then reboot).

1. Make a folder with these files (they are in
   [`docker/rtlsdrairband/example/`](docker/rtlsdrairband/example)):

   ```
   airband/
     docker-compose.yml
     .env                       READ5_UPLOAD_KEY=r5_<receiver>.<secret>
     config/
       rtl_airband.conf
       read5-uploader.toml
   ```

2. `docker-compose.yml`:

   ```yaml
   services:
     rtlsdrairband:
       image: ghcr.io/jimvella/rtlsdrairband-read5:latest
       container_name: rtlsdrairband
       restart: unless-stopped
       stop_grace_period: 30s
       device_cgroup_rules:
         - "c 189:* rwm"
       ports:
         - 8000:8000                  # Icecast, if you use it
       environment:
         - RTLSDRAIRBAND_CUSTOMCONFIG=true
         - READ5_UPLOAD_KEY=${READ5_UPLOAD_KEY}
       volumes:
         - /dev/bus/usb:/dev/bus/usb:ro
         - ./config:/run/rtlsdr-airband     # rtl_airband.conf, read5-uploader.toml
         - ./read5:/var/lib/read5           # audio waiting to upload, status.json
   ```

   Read5 needs your own `rtl_airband.conf` (`RTLSDRAIRBAND_CUSTOMCONFIG`):
   the image's frequency variables (`RTLSDRAIRBAND_FREQS` and so on) only
   produce an Icecast output. If you only feed Read5, leave the Icecast
   output out of `rtl_airband.conf`, add `- ICECAST_DISABLE=true` and drop
   the `ports`.

3. `.env`, readable only by you (`chmod 600 .env`):

   ```
   READ5_UPLOAD_KEY=r5_<receiver>.<secret>
   ```

4. `config/rtl_airband.conf`: your rtl_airband config, with one extra
   output on each channel Read5 issued you, each on its own port:

   ```
   { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6101; continuous = true; }
   ```

   `continuous = true` is required, and the device must be in
   `multichannel` mode: channels in scan mode and mixers can't be recorded.

5. `config/read5-uploader.toml`: a `[[channels]]` entry for each of those
   frequencies, with the channel id Read5 issued:

   ```toml
   [receiver]
   name = "my-receiver"

   [rtl_airband]
   config = '/run/rtlsdr-airband/rtl_airband.conf'

   [[channels]]
   khz = 118550
   id = "ypph-acd-118550"
   label = "Perth Clearance Delivery"

   [spool]
   dir = '/var/lib/read5/spool'

   [run]
   status_file = '/var/lib/read5/status.json'
   ```

6. Start it, then check the config, including that Read5 accepts the key:

   ```
   docker compose up -d
   docker exec rtlsdrairband read5-uploader check -c /run/rtlsdr-airband/read5-uploader.toml
   ```

Day to day:

| | |
|---|---|
| Logs | `docker compose logs -f` (rtl_airband's lines start `[rtlsdr-airband]`) |
| Status | `read5/status.json`, as under Running below |
| Health | `docker ps` shows the container unhealthy if the uploader stops or hangs |
| After editing either config | `docker compose restart` |
| Upgrade | `docker compose pull && docker compose up -d` |

Audio recorded while the network or Read5 is unreachable waits in the
`read5` folder and uploads later, across restarts and upgrades. Only one
uploader may run per upload key: stop a `read5-uploader` service on the same
machine before starting the container with its key.

With an rtl_airband that isn't docker-rtlsdrairband,
`ghcr.io/jimvella/read5-uploader` is this program alone. Mount the config
at `/etc/read5/read5-uploader.toml` and a volume at `/var/lib/read5`, set
`READ5_UPLOAD_KEY`, and make sure rtl_airband's datagrams reach it:
`network_mode: host` on both containers is the simplest way. If it can't
read `rtl_airband.conf`, list the streams in `[[rtl_airband.streams]]`.

## Example: one channel

A working setup from a Read5 test receiver: a Pi 4 with an RTL-SDR dongle,
recording Perth Clearance Delivery on 118.55 MHz.

`/usr/local/etc/rtl_airband.conf`:

```
devices: (
  {
    type = "rtlsdr";
    index = 0;
    gain = 32.8;
    centerfreq = 118.75;     # off the channel, so the tuner's DC spike isn't on it
    correction = 0;
    channels: (
      {
        freq = 118.55;
        label = "Perth Clearance Delivery";
        bandwidth = 10000;   # 10 kHz audio filter
        outputs: (
          { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6101; continuous = true; }
        );
      }
    );
  }
);
```

`/etc/read5/read5-uploader.toml`:

```toml
[receiver]
name = "perth-pi-airband"

[rtl_airband]
config = '/usr/local/etc/rtl_airband.conf'
listen = "127.0.0.1"
require_ntp = true

[[channels]]
khz = 118550
id = "ypph-acd-118550"
label = "Perth Clearance Delivery"

[upload]
url = 'https://read5.net/api/ingest'

[spool]
dir = '/var/lib/read5/spool'
max_backlog_minutes = 4320

[encode]
bitrate_kbps = 12

[process]
enabled = true

[run]
status_file = '/var/lib/read5/status.json'
```

`/etc/read5/read5.env` (mode 600, owned by root):

```
READ5_UPLOAD_KEY=r5_<receiver>.<secret>
```

`read5-uploader check` then prints:

```
config:   /etc/read5/read5-uploader.toml (ok)
receiver: 3
spool:    /var/lib/read5/spool
port  6101  118550 kHz  ypph-acd-118550           Perth Clearance Delivery
upload:   https://read5.net/api/ingest accepts the key
```

For more channels, add a channel to rtl_airband with its own `dest_port`
(6102, 6103, ...), and a `[[channels]]` entry here for its frequency. One
RTL-SDR dongle covers about 2.5 MHz around its `centerfreq`.

## Running

| | |
|---|---|
| Logs | `journalctl -u read5-uploader -f` |
| Status | `/var/lib/read5/status.json`: uploads waiting, and per stream whether it's up, its clock drift and jitter, and lost batches |
| Health | `read5-uploader health` exits 0 while the uploader is running and 1 if it has stopped or hung (by the age of the status file). systemd's watchdog does this for the service; it is for a Docker `HEALTHCHECK` |
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
`bash scripts/docker.sh` builds the two Docker images from the same
cross-compiled binary (`PUSH=1` for all three architectures, pushed). Built
without it, as `docker build` or Compose do from the repository alone, the
Dockerfiles compile the uploader themselves.
After changing dependencies, regenerate the licence list with
`cargo about generate about.hbs -o THIRD_PARTY_LICENSES.md`
([cargo-about](https://github.com/EmbarkStudios/cargo-about)).

## Licence

MIT, see [LICENSE](LICENSE). The binary includes libopus (BSD-3-Clause)
and Rust crates under their own permissive licences, listed in
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md). The rtl_airband
example configs in `tests/fixtures/` are GPL-3.0 test inputs, not part of
the program.
