#!/bin/sh
# SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
# SPDX-License-Identifier: AGPL-3.0-or-later

set -e

systemctl stop zorian.service || true
systemctl disable zorian.service || true
