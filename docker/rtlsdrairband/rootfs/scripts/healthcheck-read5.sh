#!/command/with-contenv bash
# shellcheck shell=bash
# docker-rtlsdrairband's own health check (its Icecast connections), and the
# uploader's when it is in use.

EXITCODE=0

if [ -n "$RTLSDRAIRBAND_CUSTOMCONFIG" ]; then
	RTLSDRAIRBAND_CONFIGFILE='/run/rtlsdr-airband/rtl_airband.conf'
else
	RTLSDRAIRBAND_CONFIGFILE='/usr/local/etc/rtl_airband.conf'
fi

# The base image's check fails when the config has no Icecast output at all
# (its grep finds nothing under `set -e`), which is a valid setup here: a
# receiver that only feeds Read5.
if grep -Eq 'type\s*=\s*"icecast"' "$RTLSDRAIRBAND_CONFIGFILE"; then
	/scripts/healthcheck.sh || EXITCODE=1
fi

if [ -n "${READ5_UPLOAD_KEY}" ]; then
	echo "READ5 UPLOADER CHECK:"
	/usr/local/bin/read5-uploader health --config "${READ5_CONFIG:-/run/rtlsdr-airband/read5-uploader.toml}" || EXITCODE=1
fi

exit "$EXITCODE"
