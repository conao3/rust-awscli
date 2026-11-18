#!/usr/bin/env bash
# A `BROWSER` for credcompare.sh: follows the authorization URL the way the
# identity provider would, by calling the CLI's loopback redirect URI with
# a code and the state from the URL. With FAKEBROWSER_PROBE set, it first
# sends the requests the callback server rejects (another path, another
# state, no code) and writes their status codes and bodies to
# ~/.aws/probe.txt.
exec 2>>"$HOME/fakebrowser.log"
set -euxo pipefail -o posix
url=$1
redirect=$(printf '%s' "$url" | sed -n 's/.*redirect_uri=\([^&]*\).*/\1/p' | sed 's/%3A/:/g; s/%2F/\//g')
state=$(printf '%s' "$url" | sed -n 's/.*[?&]state=\([^&]*\).*/\1/p')
probe() {
  curl -s -w ' %{http_code} %{content_type}\n' "$1" >>"$HOME/.aws/probe.txt"
}
(
  sleep 0.3
  if [ -n "${FAKEBROWSER_PROBE:-}" ]; then
    mkdir -p "$HOME/.aws"
    probe "${redirect%/oauth/callback}/other?code=c&state=$state"
    probe "$redirect?code=c&state=wrong"
    probe "$redirect?code=c"
    probe "$redirect?state=$state"
  fi
  curl -s -o /dev/null "$redirect?code=stub-auth-code&state=$state"
) &
exit 0
