#!/usr/bin/env bash
# Compare `awsr` with the reference AWS CLI on the configure / sso / login
# commands and the credential providers. Each case runs both CLIs in a fresh
# HOME, then compares exit codes, stdout, stderr, and every file written
# under ~/.aws (time-dependent values normalized).
#
# Usage: difftest/scripts/credcompare.sh [CASE_FILTER]
# Needs the devShell (python3, the reference `aws`) and a built awsr.
set -euxo pipefail -o posix
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
REF=${AWSR_REFERENCE_AWS:-aws}
AWSR=${AWSR_BIN:-$ROOT/target/debug/awsr}
WORK=${CREDCOMPARE_WORK:-/tmp/credcompare}
PORT=${CREDSTUB_PORT:-18799}
FILTER=${1:-}
rm -rf "$WORK"
mkdir -p "$WORK"
python3 "$ROOT/difftest/scripts/credstub.py" "$PORT" "$WORK/stub.log" &
STUB=$!
# Stubs with several accounts and roles for the configure sso menus.
CREDSTUB_ACCOUNTS=3 CREDSTUB_ROLES=2 python3 "$ROOT/difftest/scripts/credstub.py" "$((PORT + 1))" "$WORK/stub-multi.log" &
STUB_MULTI=$!
CREDSTUB_ACCOUNTS=12 python3 "$ROOT/difftest/scripts/credstub.py" "$((PORT + 2))" "$WORK/stub-many.log" &
STUB_MANY=$!
trap 'kill $STUB $STUB_MULTI $STUB_MANY 2>/dev/null' EXIT
sleep 0.5
STUBURL=http://127.0.0.1:$PORT
pass=0
failed=0

normalize() {
  sed -E -e "s#$WORK/[^/]*/(ref|awsr)#<HOME>#g" \
    -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z/<TS>/g' \
    -e 's/20[0-8][0-9]-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\+00:00/<TS>/g' \
    -e 's/botocore-session-[0-9]+/botocore-session-<N>/g' \
    -e 's/botocore-client-[0-9]+/botocore-client-<N>/g' \
    -e 's/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/<UUID>/g' \
    -e 's/127\.0\.0\.1:[0-9]+\/oauth/127.0.0.1:<PORT>\/oauth/g' \
    -e 's/127\.0\.0\.1%3A[0-9]+%2Foauth/127.0.0.1%3A<PORT>%2Foauth/g' \
    -e 's/code_challenge=[A-Za-z0-9_-]+/code_challenge=<C>/g' \
    -e 's/"dpopKey": "[^"]*"/"dpopKey": "<KEY>"/g'
}

snapshot() {
  local home=$1
  echo "== rc: $(cat "$home.rc")"
  echo "== stdout"; cat "$home.out"
  echo "== stderr"; cat "$home.err"
  if [ -d "$home/.aws" ]; then
    (cd "$home" && find .aws -type f ! -name session.db | sort | while read -r f; do
      echo "== file $f $(stat -c %a "$f")"
      cat "$f"; echo
    done)
  fi
}

# case NAME SETUP STDIN ARGS...
case_() {
  local name=$1 setup=$2 input=$3
  shift 3
  if [ -n "$FILTER" ] && [[ "$name" != *$FILTER* ]]; then return; fi
  for impl in ref awsr; do
    local home="$WORK/$name/$impl"
    mkdir -p "$home"
    local bin=$REF
    [ "$impl" = awsr ] && bin=$AWSR
    (cd "$home" && HOME=$home STUBURL=$STUBURL ROOTDIR=$ROOT bash -c "$setup")
    printf '%b' "$input" | env -i PATH="$PATH" HOME="$home" TERM=dumb AWS_EC2_METADATA_DISABLED=true \
      AWS_ENDPOINT_URL_STS=$STUBURL AWS_ENDPOINT_URL_SSO_OIDC=$STUBURL AWS_ENDPOINT_URL_SSO=$STUBURL \
      AWS_ENDPOINT_URL_SIGNIN=$STUBURL BROWSER="$ROOT/difftest/scripts/fakebrowser.sh" \
      ${CASE_ENV:-} timeout 60 setsid "$bin" "$@" >"$home.out" 2>"$home.err" \
      && echo 0 >"$home.rc" || echo $? >"$home.rc"
    snapshot "$home" | normalize >"$home.snap"
  done
  if diff -u "$WORK/$name/ref.snap" "$WORK/$name/awsr.snap" >"$WORK/$name.diff"; then
    pass=$((pass + 1))
  else
    failed=$((failed + 1))
    echo "FAIL $name"
    head -40 "$WORK/$name.diff"
  fi
}

CFG='mkdir -p .aws; cat > .aws/config'
CRED='mkdir -p .aws; cat > .aws/credentials'
WATCH='(for i in $(seq 100); do u=$(grep -o "redirect_uri=[^&]*" "$HOME.out" 2>/dev/null | head -1); s=$(grep -o "state=[^&]*" "$HOME.out" 2>/dev/null | head -1); if [ -n "$u" ]; then "$ROOTDIR/difftest/scripts/fakebrowser.sh" "x?$u&$s&"; exit; fi; sleep 0.1; done) >/dev/null 2>&1 &'

# configure get / set / list / list-profiles
case_ get-missing "" "" configure get region
case_ get-set-get "$CFG <<<'[default]
region = us-west-2
s3 =
    addressing_style = path
[profile dev]
output = text'" "" configure get s3.addressing_style
case_ get-section "$CFG <<<'[default]
s3 =
    a = b'" "" configure get s3
case_ get-dotted-profile "$CFG <<<'[profile dev]
output = text'" "" configure get profile.dev.output
case_ get-index-error "" "" configure get profile.x
case_ get-mutex "" "" configure get --sso-session a --services b x
case_ get-subsection "$CFG <<<'[sso-session s]
sso_region = us-east-1'" "" configure get --sso-session s sso_region
case_ get-subsection-missing "" "" configure get --sso-session s sso_region
case_ get-noprofile "" "" --profile nope configure get region
case_ set-new "" "" configure set region us-west-2
case_ set-nested "$CFG <<<'[default]
region = x
s3 =
    a = 1
output = json'" "" configure set s3.b 2
case_ set-profile "$CFG <<<'# comment
[default]
region = a'" "" configure set profile.dev.region eu-west-1
case_ set-creds "" "" --profile p configure set aws_access_key_id AKID
case_ set-subsection "" "" configure set --sso-session my-sso sso_region us-east-1
case_ set-subsection-nested "" "" configure set --services svc s3.endpoint_url http://x
case_ set-missing-args "" "" configure set region
case_ set-quoted "$CFG <<<'[profile \"my prof\"]
region = a'" "" --profile 'my prof' configure set region b
case_ set-noeol "printf '[default]\nregion = a' > .aws/config 2>/dev/null || (mkdir -p .aws; printf '[default]\nregion = a' > .aws/config)" "" configure set output json
case_ set-newsection-noeol "mkdir -p .aws; printf '[default]\nregion = a' > .aws/config" "" --profile x configure set output json
case_ list-empty "" "" configure list
case_ list-env "" "" configure list
case_ list-profile "$CFG <<<'[profile dev]
region = eu-west-1'
$CRED <<<'[dev]
aws_access_key_id = AKIAEXAMPLE1234
aws_secret_access_key = secretsecret'" "" --profile dev configure list
case_ list-noprofile "" "" --profile nope configure list
case_ list-profiles "$CFG <<<'[default]
[profile b]
[profile a]
[sso-session s]'
$CRED <<<'[c]
[a]'" "" configure list-profiles
case_ bad-subcommand "" "" configure bogus
case_ unknown-option "" "" configure list --foo
case_ unknown-option-suggest "" "" configure get --profil x y

# interactive configure
case_ interactive-new "" 'AKIDEXAMPLE\nSECRET\nus-west-2\njson\n' configure
case_ interactive-update "$CFG <<<'[default]
region = us-west-2
output = json'
$CRED <<<'[default]
aws_access_key_id = AKIDEXAMPLE
aws_secret_access_key = SECRET'" '\n\neu-west-1\n\n' configure
case_ interactive-asia "" 'ASIAX\nS\nTOK\n\n\n' configure
case_ interactive-switch-long "$CRED <<<'[default]
aws_access_key_id = ASIAOLD
aws_secret_access_key = S
aws_session_token = T'" 'AKIANEW\n\n\n\n' configure
case_ interactive-eof "" 'AK\n' configure
case_ interactive-profile "" 'AK\nSK\n\ntext\n' --profile newp configure

# export-credentials
case_ export-none "" "" configure export-credentials
case_ export-process "$CRED <<<'[default]
aws_access_key_id = AKID
aws_secret_access_key = SECRET
aws_session_token = TOKEN'" "" configure export-credentials
for fmt in env env-no-export powershell windows-cmd fish; do
  case_ export-$fmt "$CRED <<<'[default]
aws_access_key_id = AKID
aws_secret_access_key = SECRET'" "" configure export-credentials --format $fmt
done
case_ export-bad-format "" "" configure export-credentials --format bad
case_ export-process-provider "$CFG <<<'[default]
credential_process = printf {\"Version\":1,\"AccessKeyId\":\"PK\",\"SecretAccessKey\":\"PS\",\"Expiration\":\"2099-01-01T00:00:00Z\"}'" "" configure export-credentials --format env
case_ export-process-fail "$CFG <<<'[default]
credential_process = sh -c \"echo boom >&2; exit 1\"'" "" configure export-credentials
case_ export-process-version "$CFG <<<'[default]
credential_process = printf {\"Version\":2}'" "" configure export-credentials
case_ export-recursion "$CFG <<<'[default]
credential_process = aws configure export-credentials'" "" configure export-credentials

# import
case_ import-csv "printf 'User Name,Access key ID,Secret access key\nalice,AKIA1,S1\nbob,AKIA2,S2\n' > creds.csv" "" configure import --csv file://creds.csv
case_ import-prefix "printf 'User name,Access key ID,Secret access key\nalice,AKIA1,S1\n' > creds.csv" "" configure import --csv file://creds.csv --profile-prefix p-
case_ import-invalid "printf 'User name,Access key ID,Secret access key\nalice,,S1\n' > creds.csv" "" configure import --csv file://creds.csv
case_ import-skip "printf 'User name,Access key ID,Secret access key\nalice,,S1\nbob,AK,SK\n' > creds.csv" "" configure import --csv file://creds.csv --skip-invalid
case_ import-noheader "printf 'a,b\n' > creds.csv" "" configure import --csv file://creds.csv
case_ import-missing "" "" configure import

# assume role and its cache
ROLE="$CFG <<<'[profile src]
aws_access_key_id = AKIDSRC
aws_secret_access_key = SECRETSRC
[profile role]
role_arn = arn:aws:iam::123456789012:role/r
source_profile = src
region = us-east-1
duration_seconds = 3600
[profile mfa]
role_arn = arn:aws:iam::123456789012:role/m
source_profile = src
mfa_serial = arn:aws:iam::123456789012:mfa/me
region = us-east-1
[profile chain]
role_arn = arn:aws:iam::123456789012:role/c
source_profile = role
region = us-east-1
[profile loop]
role_arn = arn:aws:iam::123456789012:role/l
source_profile = loop
region = us-east-1
[profile both]
role_arn = arn:aws:iam::123456789012:role/b
source_profile = src
credential_source = Environment
[profile neither]
role_arn = arn:aws:iam::123456789012:role/n
[profile badsource]
role_arn = arn:aws:iam::123456789012:role/n
credential_source = Nope
[profile missingsrc]
role_arn = arn:aws:iam::123456789012:role/n
source_profile = ghost'"
case_ assume-role "$ROLE" "" --profile role sts get-caller-identity
case_ assume-role-list "$ROLE" "" --profile role configure list
case_ assume-role-export "$ROLE" "" --profile role configure export-credentials
case_ assume-role-chain "$ROLE" "" --profile chain configure export-credentials
case_ assume-role-mfa "$ROLE" "123456\n" --profile mfa configure export-credentials
case_ assume-role-mfa-denied "$ROLE" "000000\n" --profile mfa sts get-caller-identity
case_ assume-role-loop "$ROLE" "" --profile loop configure export-credentials
case_ assume-role-both "$ROLE" "" --profile both sts get-caller-identity
case_ assume-role-neither "$ROLE" "" --profile neither sts get-caller-identity
case_ assume-role-badsource "$ROLE" "" --profile badsource sts get-caller-identity
case_ assume-role-missingsrc "$ROLE" "" --profile missingsrc sts get-caller-identity
case_ assume-role-cached "$ROLE
mkdir -p .aws/cli/cache
printf '%s' '{\"Credentials\": {\"AccessKeyId\": \"ASIACACHED\", \"SecretAccessKey\": \"cs\", \"SessionToken\": \"ct\", \"Expiration\": \"2099-01-01T00:00:00+00:00\", \"AccountId\": \"123456789012\"}}' > .aws/cli/cache/5b4f30aca379fe07625746c18f3f425bb3231f15.json" "" --profile role configure export-credentials
case_ web-identity "echo -n webtoken > token; $CFG <<<'[default]
web_identity_token_file = '\$HOME'/token
role_arn = arn:aws:iam::123456789012:role/w
role_session_name = s
region = us-east-1'" "" configure export-credentials

# sso credentials
SSO="$CFG <<<'[profile sso]
sso_session = s
sso_account_id = 111111111110
sso_role_name = Role0
region = us-east-1
[profile legacy]
sso_start_url = https://d-1.awsapps.com/start
sso_region = us-east-1
sso_account_id = 111111111110
sso_role_name = Role0
[profile partial]
sso_account_id = 1
[sso-session s]
sso_start_url = https://d-1.awsapps.com/start
sso_region = us-east-1'
mkdir -p .aws/sso/cache
printf '%s' '{\"startUrl\": \"https://d-1.awsapps.com/start\", \"region\": \"us-east-1\", \"accessToken\": \"tok\", \"expiresAt\": \"2099-01-01T00:00:00Z\"}' > .aws/sso/cache/\$(printf s | sha1sum | cut -c1-40).json
printf '%s' '{\"startUrl\": \"https://d-1.awsapps.com/start\", \"region\": \"us-east-1\", \"accessToken\": \"tok\", \"expiresAt\": \"2099-01-01T00:00:00Z\"}' > .aws/sso/cache/\$(printf https://d-1.awsapps.com/start | sha1sum | cut -c1-40).json"
case_ sso-creds "$SSO" "" --profile sso configure export-credentials
case_ sso-creds-legacy "$SSO" "" --profile legacy configure export-credentials
case_ sso-creds-partial "$SSO" "" --profile partial configure export-credentials
case_ sso-logout "$SSO" "" --profile sso sso logout
case_ sso-login-device "$SSO" "" --profile sso sso login --use-device-code --no-browser
case_ sso-login-device-legacy "$SSO" "" --profile legacy sso login --no-browser
case_ sso-login-pkce "$SSO" "" --profile sso sso login
case_ sso-login-pkce-nobrowser "$SSO
$WATCH" "" --profile sso sso login --no-browser
case_ sso-login-missing "" "" sso login
case_ sso-login-badport "$SSO" "" --profile sso sso login --redirect-port 0
case_ sso-login-noport "$SSO" "" --profile sso sso login --redirect-port abc
case_ sso-session-configure "" 'mysess\nhttps://d-1.awsapps.com/start\nus-east-1\n\n' configure sso-session
case_ sso-session-eof "" 'x\n' configure sso-session
case_ configure-sso "" 'mysess\nhttps://d-1.awsapps.com/start\nus-east-1\n\nus-west-2\njson\nmyprof\n' configure sso --use-device-code --no-browser
SSO_IN='mysess\nhttps://d-1.awsapps.com/start\nus-east-1\n\n'
MULTI="AWS_ENDPOINT_URL_SSO_OIDC=http://127.0.0.1:$((PORT + 1)) AWS_ENDPOINT_URL_SSO=http://127.0.0.1:$((PORT + 1))"
MANY="AWS_ENDPOINT_URL_SSO_OIDC=http://127.0.0.1:$((PORT + 2)) AWS_ENDPOINT_URL_SSO=http://127.0.0.1:$((PORT + 2))"
CASE_ENV=$MULTI case_ configure-sso-accounts-eof "" "$SSO_IN" configure sso --use-device-code --no-browser
CASE_ENV=$MULTI case_ configure-sso-accounts-enter "" "${SSO_IN}\r" configure sso --use-device-code --no-browser
CASE_ENV=$MULTI case_ configure-sso-accounts-down "" "${SSO_IN}x\\x1b[B\\x1b[B\r\r" configure sso --use-device-code --no-browser
CASE_ENV=$MULTI case_ configure-sso-accounts-wrap "" "${SSO_IN}\\x1b[A\\x1b[B\\x1b[B\\x1b[B\\x1b[B\r" configure sso --use-device-code --no-browser
CASE_ENV=$MULTI case_ configure-sso-accounts-newline "" "${SSO_IN}\n\n" configure sso --use-device-code --no-browser
CASE_ENV=$MULTI case_ configure-sso-accounts-interrupt "" "${SSO_IN}\\x03" configure sso --use-device-code --no-browser
CASE_ENV=$MANY case_ configure-sso-accounts-scroll "" "${SSO_IN}\\x1b[A\r" configure sso --use-device-code --no-browser
CASE_ENV=$MANY case_ configure-sso-accounts-many-eof "" "${SSO_IN}" configure sso --use-device-code --no-browser
case_ configure-sso-legacy "" '\nhttps://d-1.awsapps.com/start\nus-east-1\n\n\n\n' configure sso --use-device-code --no-browser

# login / logout
case_ logout-none "" "" logout
case_ logout-all-none "" "" logout --all
case_ login-existing-creds "$CRED <<<'[default]
aws_access_key_id = A
aws_secret_access_key = B'" "" --region us-east-1 login --remote

case_ login-same-device "" "" --region us-east-1 login
case_ login-same-device-profile "$CFG <<<'[profile p]
region = eu-west-1'" "" --profile p login
case_ login-remote-badcode "" 'bm90LWJhc2U2NA\n' --region us-east-1 login --remote
LOGIN="$CFG <<<'[profile l]
login_session = arn:aws:signin:us-east-1:123456789012:login-session/stub
region = us-east-1'
mkdir -p .aws/login/cache
key=\$(python3 -c 'from awscrt.crypto import EC,ECType,ECExportFormat;import base64,textwrap;k=EC.new_generate(ECType.P_256);print(\"-----BEGIN EC PRIVATE KEY-----\\\\n\"+\"\\\\n\".join(textwrap.wrap(base64.b64encode(k.export_key(ECExportFormat.SEC1)).decode(),64))+\"\\\\n-----END EC PRIVATE KEY-----\")') # gitleaks:allow
f=.aws/login/cache/\$(printf %s arn:aws:signin:us-east-1:123456789012:login-session/stub | sha256sum | cut -c1-64).json
printf '{\"accessToken\": {\"accessKeyId\": \"ASIAL\", \"secretAccessKey\": \"ls\", \"sessionToken\": \"lt\", \"accountId\": \"123456789012\", \"expiresAt\": \"%s\"}, \"tokenType\": \"aws_sigv4\", \"clientId\": \"arn:aws:signin:::devtools/same-device\", \"refreshToken\": \"r\", \"idToken\": \"i\", \"dpopKey\": \"%s\\\\n\", \"signInRegion\": \"us-east-1\"}' \"\$EXP\" \"\$key\" > \$f"
case_ login-creds-cached "EXP=2099-01-01T00:00:00Z; $LOGIN" "" --profile l configure export-credentials
case_ login-creds-refresh "EXP=2020-01-01T00:00:00Z; $LOGIN" "" --profile l configure export-credentials
case_ logout-profile "EXP=2099-01-01T00:00:00Z; $LOGIN" "" --profile l logout
case_ logout-all "EXP=2099-01-01T00:00:00Z; $LOGIN" "" logout --all
echo "passed: $pass failed: $failed"
