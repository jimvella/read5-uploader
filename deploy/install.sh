#!/bin/sh
# Install or upgrade read5-uploader on a Raspberry Pi running rtl_airband.
# Run as root from the unpacked bundle:  sudo ./install.sh
# Upgrading replaces the binary and the unit, keeps /etc/read5, and restarts
# the service if it was running.
set -eu
cd "$(dirname "$0")"
[ "$(id -u)" = 0 ] || { echo "run as root: sudo $0" >&2; exit 1; }

id read5 >/dev/null 2>&1 || useradd --system --home-dir /var/lib/read5 --no-create-home --shell /usr/sbin/nologin read5

install -d -m 755 /opt/read5 /etc/read5
running=no
systemctl is-active --quiet read5-uploader && running=yes
[ "$running" = yes ] && systemctl stop read5-uploader
install -m 755 read5-uploader /opt/read5/read5-uploader
install -m 644 read5-uploader.service /etc/systemd/system/read5-uploader.service

if [ ! -f /etc/read5/read5-uploader.toml ]; then
  install -m 644 read5-uploader.toml /etc/read5/read5-uploader.toml
  fresh=yes
else
  fresh=no
fi
if [ ! -f /etc/read5/read5.env ]; then
  (
    umask 077
    cat >/etc/read5/read5.env <<'EOF'
# The upload key Read5 issued for this receiver (read by systemd, root only).
READ5_UPLOAD_KEY=
EOF
  )
fi

systemctl daemon-reload
systemctl enable read5-uploader >/dev/null

if [ "$running" = yes ] || [ "$fresh" = no ]; then
  systemctl start read5-uploader
  echo "read5-uploader upgraded and started; logs: journalctl -u read5-uploader -f"
else
  cat <<'EOF'
Installed. Before starting:
  1. In /usr/local/etc/rtl_airband.conf, add to each channel's outputs:
       { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6101; continuous = true; }
     (a different port per channel), then: sudo systemctl restart rtl_airband
  2. Put the upload key in /etc/read5/read5.env, and the channels Read5
     issued with it in /etc/read5/read5-uploader.toml ([[channels]]).
  3. Check everything:
       sudo sh -c 'set -a; . /etc/read5/read5.env; /opt/read5/read5-uploader check'
  4. sudo systemctl start read5-uploader
     journalctl -u read5-uploader -f      status: /var/lib/read5/status.json
EOF
fi
