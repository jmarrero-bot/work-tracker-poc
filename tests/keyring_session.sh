#!/bin/sh
# Run a command with GNOME Keyring on the session bus, its login keyring
# made and unlocked with a test password, as a desktop login does, so the
# Secret Service has a default collection and never prompts.
set -eu
mkdir -p "$HOME/.local/share/keyrings"
printf 'workboard-test\n' | gnome-keyring-daemon --unlock --components=secrets >/dev/null
gnome-keyring-daemon --start --components=secrets >/dev/null
exec "$@"
