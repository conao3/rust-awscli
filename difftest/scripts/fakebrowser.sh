#!/usr/bin/env bash
# A `BROWSER` for credcompare.sh: follows the authorization URL the way the
# identity provider would, by calling the CLI's loopback redirect URI with
# a code and the state from the URL.
url=$1
redirect=$(printf '%s' "$url" | sed -n 's/.*redirect_uri=\([^&]*\).*/\1/p' | sed 's/%3A/:/g; s/%2F/\//g')
state=$(printf '%s' "$url" | sed -n 's/.*[?&]state=\([^&]*\).*/\1/p')
(sleep 0.3; curl -s -o /dev/null "$redirect?code=stub-auth-code&state=$state") &
exit 0
