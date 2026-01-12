#!/bin/sh
# SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
# SPDX-License-Identifier: AGPL-3.0-or-later

set -e

PROGRAM=zorian
ZORIAN_USER=${ZORIAN_USER:-zorian}
ZORIAN_GROUP=${ZORIAN_GROUP:-${ZORIAN_USER}}

if ! getent group $ZORIAN_GROUP >/dev/null; then
  groupadd --system $ZORIAN_GROUP
fi

if ! getent passwd $ZORIAN_USER >/dev/null; then
  useradd --system --gid $ZORIAN_GROUP --no-create-home --shell /usr/sbin/nologin $ZORIAN_USER
fi

if [ -x "/bin/systemctl" ] && [ -d /run/systemd/system ] && [ -f /usr/lib/systemd/system/zorian.service ]; then
  /bin/systemctl daemon-reload
  /bin/systemctl enable zorian
fi
