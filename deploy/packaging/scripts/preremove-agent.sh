#!/bin/sh
# Before removal (not upgrades): stop the service. Debian passes "remove",
# RPM passes 0.
set -e
case "$1" in
  remove|0)
    if [ -d /run/systemd/system ]; then
      systemctl disable --now mst-agent.service || true
    fi
    ;;
esac
