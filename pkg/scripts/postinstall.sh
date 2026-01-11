#!/bin/sh
# SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
# SPDX-License-Identifier: AGPL-3.0-or-later

set -e

if ! getent group zorian >/dev/null; then
    groupadd --system zorian
fi

if ! getent passwd zorian >/dev/null; then
    useradd --system --gid zorian --no-create-home --shell /usr/sbin/nologin zorian
fi

systemctl daemon-reload
