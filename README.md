# Zorian — tiny packages caching proxy

Soon humanity will go to Mars and in order for the colonists to be able to program, we will need a local mirror of packages.

## Installation

Please note that the project is in its infancy and is **not** intended for production use.

**Use pre-bundled dev build:**

1. Download the package for your distribution from the [releases page](https://github.com/mrdimidium/Zorian/releases/tag/dev)
1. Install via system package manager:

    ```bash
    # Debian/Ubuntu
    sudo dpkg -i zorian_*.deb
    # Fedora
    sudo dnf install zorian-*.rpm
    # Arch Linux
    sudo pacman -U zorian-*.pkg.tar.zst
    ```

1. start systemd service: `sudo systemctl enable --now zorian`

**Build from source:**

1. Install build dependencies:
    - Arch Linux: `sudo pacman -S base-devel openssl pkg-config`
    - Fedora/RedHat: `sudo dnf install gcc openssl-devel pkg-config`
    - Debian/Ubuntu: `sudo apt install build-essential libssl-dev pkg-config`
1. Clone and build via cargo:

    ```bash
    git clone https://github.com/mrdimidium/Zorian.git && cd Zorian

    cargo build --release

    # Build linux package packages via nfpm (ihttps://nfpm.goreleaser.com/)
    nfpm package -p deb --target dist/
    nfpm package -p rpm --target dist/
    nfpm package -p archlinux --target dist/

    # Install binary manually
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
