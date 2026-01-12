#!/bin/sh
# SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
# SPDX-License-Identifier: AGPL-3.0-or-later

set -e

if [ -x "/bin/systemctl" ] && [ -d /run/systemd/system ] && [ -f /usr/lib/systemd/system/zorian.service ]; then
  /bin/systemctl stop zorian.service || true
  /bin/systemctl disable zorian.service || true
fi
