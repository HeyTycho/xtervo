#!/bin/sh
prompt="$1"
case "$prompt" in
  *[Pp]assword*|*密码*)
    if [ -n "$TERVO_ASKPASS_DEBUG" ]; then
      printf '%s\n' "match=password prompt=$prompt" >>"$TERVO_ASKPASS_DEBUG" 2>/dev/null
    fi
    printf '%s\n' "$TERVO_ASKPASS_SECRET"
    ;;
  *)
    if [ -n "$TERVO_ASKPASS_DEBUG" ]; then
      printf '%s\n' "match=no prompt=$prompt" >>"$TERVO_ASKPASS_DEBUG" 2>/dev/null
    fi
    printf 'no\n'
    ;;
esac
