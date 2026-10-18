#!/usr/bin/env bash
# Adsum uninstaller. Stops + disables the user service and removes what
# install.sh laid down. Config, caches and backups are kept unless
# --purge is given.
#
#   ./scripts/uninstall.sh [--purge] [--yes]
set -euo pipefail

APP="adsum"
UNIT_NAME="adsum.service"
BIN_DIR="${HOME}/.local/bin"
UNIT_DIR="${HOME}/.config/systemd/user"

PURGE=0 YES=0

while [ $# -gt 0 ]; do
  case "$1" in
    --purge) PURGE=1 ;;
    --yes) YES=1 ;;
    -h|--help)
      echo "Usage: uninstall.sh [--purge] [--yes]"
      echo "  --purge  also remove config (~/.config/adsum), caches (~/.cache/adsum) and .bak-* backups"
      exit 0
      ;;
    *) echo "[uninstall] ERROR: unknown argument: $1" >&2; exit 1 ;;
  esac
  shift
done

log() { printf '[uninstall] %s\n' "$*"; }

if [ "$YES" -eq 0 ] && [ "$PURGE" -eq 0 ]; then
  printf '[uninstall] remove the binary and unit (config/caches kept)? [y/N] ' >&2
  answer=""
  read -r answer </dev/tty 2>/dev/null || { log "cancelled"; exit 0; }
  case "$answer" in
    y|Y|yes|YES) ;;
    *) log "cancelled"; exit 0 ;;
  esac
fi

systemctl --user stop "$UNIT_NAME" 2>/dev/null || true
systemctl --user disable "$UNIT_NAME" 2>/dev/null || true

rm -f "${UNIT_DIR}/${UNIT_NAME}" "${BIN_DIR}/${APP}"
log "removed unit and binary"

if [ "$PURGE" -eq 1 ]; then
  rm -rf "${UNIT_DIR}/${UNIT_NAME}.d" "${HOME}/.config/adsum" "${HOME}/.cache/adsum"
  rm -f "${BIN_DIR}/${APP}".bak-* "${UNIT_DIR}/${UNIT_NAME}".bak-*
  log "purged config, caches, drop-ins and backups"
else
  log "kept: config (~/.config/adsum), caches (~/.cache/adsum), .bak-* backups"
fi

systemctl --user daemon-reload 2>/dev/null || true
log "done"
