#!/bin/sh
# After installing or upgrading: systemd learns of the unit. A running
# service restarts on the new binary; a new install waits for its
# configuration (see /usr/share/doc/metastratum-controller).
set -e
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  systemctl try-restart mst-controller.service
fi
if [ ! -e /etc/metastratum/controller.toml ]; then
  echo "metastratum-controller: write /etc/metastratum/controller.toml (example in /usr/share/doc/metastratum-controller/examples), then: systemctl enable --now mst-controller"
fi
