#!/bin/sh
# cc-telegram installer: runs the cargo-dist installer from the latest GitHub release.
# It detects OS/arch, downloads the prebuilt binary, verifies its sha256 and installs
# it to ~/.local/bin (no sudo), printing a PATH hint if needed.
#
#   curl -fsSL https://raw.githubusercontent.com/opencraftdev/cc-telegram/main/install.sh | sh
set -eu
url="https://github.com/opencraftdev/cc-telegram/releases/latest/download/cc-telegram-installer.sh"
curl --proto '=https' --tlsv1.2 -LsSf "$url" | sh
echo
echo "Next: cc-telegram setup"
