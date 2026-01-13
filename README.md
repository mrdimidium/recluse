# Zorian — tiny packages caching proxy

Soon humanity will go to Mars and in order for the colonists to be able
to program, we will need a local mirror of packages.

## Installation

Please note that the project is in its infancy and
is **not** intended for production use.

**Debian/Ubuntu:**

```bash
sudo apt install curl gnupg

curl -fsSL https://github.com/mrdimidium/Zorian/releases/download/dev/public.gpg | sudo gpg --dearmor -o /usr/share/keyrings/zorian.gpg
echo "deb [signed-by=/usr/share/keyrings/zorian.gpg] https://zorian.hel1.your-objectstorage.com/apt/ dev main" | sudo tee /etc/apt/sources.list.d/zorian.list
sudo apt update && sudo apt install zorian

sudo systemctl enable --now zorian
```

**Fedora/RedHat:**

```bash
sudo rpm --import https://github.com/mrdimidium/Zorian/releases/download/dev/public.gpg
sudo dnf config-manager --add-repo https://zorian.hel1.your-objectstorage.com/rpm/
sudo dnf install zorian

sudo systemctl enable --now zorian
```

## Build from source

1. Install build dependencies:
    - Fedora/RedHat: `sudo dnf install gcc openssl-devel pkg-config`
    - Debian/Ubuntu: `sudo apt install build-essential libssl-dev pkg-config`
1. Clone repo: `git clone https://github.com/mrdimidium/Zorian.git && cd Zorian`
1. Build from source: `cargo build --release`
1. Install manually

    ```bash
    sudo groupadd --system zorian
    sudo useradd --system --gid zorian --no-create-home --shell /usr/sbin/nologin zorian
    sudo install -m 700 -o zorian ./pkg/zorian.toml     /etc/
    sudo install -m 755 -o root   ./pkg/zorian.service  /usr/lib/systemd/system
    sudo install -m 755 -o root   target/release/zorian /usr/local/bin/
    ```

1. start systemd service: `sudo systemctl enable --now zorian`

## License

Copyright (C) 2026 Nikolay Govorov

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero General Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your option) any
later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License along
with this program. If not, see <https://www.gnu.org/licenses/>.
