#!/bin/sh
printf '%s\n' "ASKPASS-CALLED prompt=$1" >> /home/yt/AIworkspace/rusttabbyterminal/.vaultssh2-e2e/ssh/askpass-debug.log
case "$1" in
  *[Pp]assword*|*密码*) printf '%s\n' "$TERVO_ASKPASS_SECRET" ;;
  *) printf 'no\n' ;;
esac
