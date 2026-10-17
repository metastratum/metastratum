#!/bin/sh
# After installing or upgrading: systemd learns of the unit. A running
# service restarts on the new binary; a new install waits for its
# configuration (see /usr/share/doc/metastratum-agent).
set -e
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  systemctl try-restart mst-agent.service
fi
if [ ! -e /etc/metastratum/agent.toml ]; then
  echo "metastratum-agent: write /etc/metastratum/agent.toml (example in /usr/share/doc/metastratum-agent/examples), then: systemctl enable --now mst-agent"
fi
