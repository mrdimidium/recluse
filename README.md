# Recluse — tiny packages caching proxy

Soon humanity will go to Mars and in order for the colonists to be able
to program, we will need a local mirror of packages.

## Installation

Please note that the project is in its infancy and
is **not** intended for production use.

**Debian/Ubuntu:**

```bash
sudo apt install curl gnupg

curl -fsSL https://github.com/dimidiumlabs/recluse/releases/download/nightly/public.gpg | sudo gpg --dearmor -o /usr/share/keyrings/recluse.gpg
echo "deb [signed-by=/usr/share/keyrings/recluse.gpg] https://dimidiumlabs.fsn1.your-objectstorage.com/apt/ nightly main" | sudo tee /etc/apt/sources.list.d/recluse.list
sudo apt update && sudo apt install recluse

sudo systemctl enable --now recluse
```

**Fedora/RHEL:**

```bash
# DNF5 (Fedora 41+, RHEL 10+)
sudo dnf config-manager addrepo --from-repofile=https://dimidiumlabs.fsn1.your-objectstorage.com/rpm/recluse-nightly.repo

# DNF4 (Fedora 40 and older, RHEL 8/9)
sudo curl -o /etc/yum.repos.d/recluse-nightly.repo https://dimidiumlabs.fsn1.your-objectstorage.com/rpm/recluse-nightly.repo

sudo dnf install recluse
sudo systemctl enable --now recluse
```

**openSUSE:**

```bash
sudo rpm --import https://github.com/dimidiumlabs/recluse/releases/download/nightly/public.gpg
sudo zypper addrepo https://dimidiumlabs.fsn1.your-objectstorage.com/rpm/ recluse-nightly
sudo zypper refresh
sudo zypper install recluse

sudo systemctl enable --now recluse
```

## Build from source

1. Install build dependencies:
    - Fedora/RedHat: `sudo dnf install gcc openssl-devel pkg-config`
    - Debian/Ubuntu: `sudo apt install build-essential libssl-dev pkg-config`
1. Clone repo: `git clone https://github.com/dimidiumlabs/recluse.git && cd recluse`
1. Build from source: `cargo build --release`
1. Install manually

    ```bash
    sudo groupadd --system recluse
    sudo useradd --system --gid recluse --no-create-home --shell /usr/sbin/nologin recluse
    sudo install -m 700 -o recluse ./pkg/recluse.toml     /etc/
    sudo install -m 755 -o root   ./pkg/recluse.service  /usr/lib/systemd/system
    sudo install -m 755 -o root   target/release/recluse /usr/local/bin/
    ```

1. start systemd service: `sudo systemctl enable --now recluse`

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
