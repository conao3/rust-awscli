"""A local stub of the credential services (STS, SSO OIDC, SSO portal,
Sign-In) for comparing `awsr` with the reference AWS CLI.

Usage: python3 credstub.py PORT LOGFILE

Every request is appended to LOGFILE as one JSON line (method, path,
query, selected headers, body). Responses are fixed and carry no `Date`
or `Server` header so that cached responses are byte-stable.

Behavior switches (environment variables of the stub process):
  CREDSTUB_EXPIRATION   STS / Sign-In credential expiration
                        (default 2099-01-01T00:00:00Z)
  CREDSTUB_PENDING      number of CreateToken calls answered with
                        AuthorizationPendingException (default 0)
  CREDSTUB_ACCOUNTS     number of SSO accounts (default 1)
  CREDSTUB_ROLES        number of SSO roles per account (default 1)
"""

import json
import os
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

EXPIRATION = os.environ.get("CREDSTUB_EXPIRATION", "2099-01-01T00:00:00Z")
PENDING = int(os.environ.get("CREDSTUB_PENDING", "0"))
ACCOUNTS = int(os.environ.get("CREDSTUB_ACCOUNTS", "1"))
ROLES = int(os.environ.get("CREDSTUB_ROLES", "1"))
STATE = {"pending": PENDING}

LOGGED_HEADERS = (
    "content-type",
    "authorization",
    "x-amz-sso_bearer_token",
    "dpop",
    "x-amz-security-token",
)


def sts_credentials(action, form):
    role = form.get("RoleArn", ["arn:aws:iam::0:user/session"])[0]
    n = role.rsplit("/", 1)[-1]
    user = (
        f"<AssumedRoleUser><AssumedRoleId>AROA:sess</AssumedRoleId>"
        f"<Arn>arn:aws:sts::123456789012:assumed-role/role/sess</Arn>"
        f"</AssumedRoleUser>"
    )
    extra = ""
    if action == "AssumeRoleWithWebIdentity":
        extra = (
            "<SubjectFromWebIdentityToken>sub</SubjectFromWebIdentityToken>"
            "<Provider>example.com</Provider><Audience>aud</Audience>"
        )
    if action == "GetSessionToken":
        user = ""
    return (
        f'<{action}Response xmlns="https://sts.amazonaws.com/doc/2011-06-15/">'
        f"<{action}Result>{extra}{user}<Credentials>"
        f"<AccessKeyId>ASIASTUB{action.upper()[:8]}{n}</AccessKeyId>"
        f"<SecretAccessKey>secret{n}</SecretAccessKey>"
        f"<SessionToken>token{n}</SessionToken>"
        f"<Expiration>{EXPIRATION}</Expiration>"
        f"</Credentials></{action}Result>"
        f"<ResponseMetadata><RequestId>req-{action}</RequestId></ResponseMetadata>"
        f"</{action}Response>"
    )


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _send(self, status, body, content_type="application/json", headers=()):
        if isinstance(body, (dict, list)):
            body = json.dumps(body)
        data = body.encode()
        self.send_response_only(status)
        self.send_header("Content-Type", content_type)
        self.send_header("x-amzn-RequestId", "stub-request-id")
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _log(self, body):
        parsed = urllib.parse.urlparse(self.path)
        entry = {
            "method": self.command,
            "path": parsed.path,
            "query": parsed.query,
            "headers": {
                k: self.headers[k] for k in LOGGED_HEADERS if self.headers.get(k)
            },
            "body": body.decode("utf-8", "replace"),
        }
        with open(sys.argv[2], "a") as f:
            f.write(json.dumps(entry, sort_keys=True) + "\n")
        return parsed

    def do_GET(self):
        parsed = self._log(b"")
        q = urllib.parse.parse_qs(parsed.query)
        if parsed.path == "/federation/credentials":
            if self.headers.get("x-amz-sso_bearer_token") == "expired":
                return self._send(
                    401,
                    {"message": "Session token not found or invalid"},
                    headers=[("x-amzn-ErrorType", "UnauthorizedException")],
                )
            return self._send(
                200,
                {
                    "roleCredentials": {
                        "accessKeyId": "ASIASSOSTUB",
                        "secretAccessKey": "ssosecret",
                        "sessionToken": "ssotoken",
                        "expiration": 4070908800000,
                    }
                },
            )
        if parsed.path == "/assignment/accounts":
            accounts = [
                {
                    "accountId": f"11111111111{i}",
                    "accountName": f"account-{i}",
                    "emailAddress": f"a{i}@example.com",
                }
                for i in range(ACCOUNTS)
            ]
            return self._send(200, {"accountList": accounts})
        if parsed.path == "/assignment/roles":
            account = q.get("account_id", [""])[0]
            roles = [
                {"roleName": f"Role{i}", "accountId": account} for i in range(ROLES)
            ]
            return self._send(200, {"roleList": roles})
        self._send(404, {"message": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length)
        parsed = self._log(body)
        path = parsed.path
        if path == "/":
            form = urllib.parse.parse_qs(body.decode())
            action = form.get("Action", [""])[0]
            if action == "GetCallerIdentity":
                return self._send(
                    200,
                    '<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">'
                    "<GetCallerIdentityResult><Arn>arn:aws:iam::123456789012:user/stub</Arn>"
                    "<UserId>AIDASTUB</UserId><Account>123456789012</Account>"
                    "</GetCallerIdentityResult><ResponseMetadata><RequestId>req</RequestId>"
                    "</ResponseMetadata></GetCallerIdentityResponse>",
                    "text/xml",
                )
            if form.get("TokenCode", [""])[0] == "000000":
                return self._send(
                    403,
                    '<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">'
                    "<Error><Type>Sender</Type><Code>AccessDenied</Code>"
                    "<Message>MultiFactorAuthentication failed with invalid MFA one time pass code.</Message>"
                    "</Error><RequestId>req</RequestId></ErrorResponse>",
                    "text/xml",
                )
            return self._send(200, sts_credentials(action, form), "text/xml")
        if path == "/client/register":
            return self._send(
                200,
                {
                    "clientId": "stub-client-id",
                    "clientSecret": "stub-client-secret",
                    "clientIdIssuedAt": 1700000000,
                    "clientSecretExpiresAt": 4070908800,
                },
            )
        if path == "/device_authorization":
            return self._send(
                200,
                {
                    "deviceCode": "stub-device-code",
                    "userCode": "ABCD-EFGH",
                    "verificationUri": "https://device.sso.us-east-1.amazonaws.com/",
                    "verificationUriComplete": "https://device.sso.us-east-1.amazonaws.com/?user_code=ABCD-EFGH",
                    "expiresIn": 600,
                    "interval": 1,
                },
            )
        if path == "/token":
            req = json.loads(body or b"{}")
            if req.get("grantType") == "urn:ietf:params:oauth:grant-type:device_code" and STATE["pending"] > 0:
                STATE["pending"] -= 1
                return self._send(
                    400,
                    {"error": "authorization_pending"},
                    headers=[("x-amzn-ErrorType", "AuthorizationPendingException")],
                )
            return self._send(
                200,
                {
                    "accessToken": "stub-access-token",
                    "tokenType": "Bearer",
                    "expiresIn": 28800,
                    "refreshToken": "stub-refresh-token",
                },
            )
        if path == "/logout":
            return self._send(200, "")
        if path == "/v1/token":
            return self._send(
                200,
                {
                    "accessToken": {
                        "accessKeyId": "ASIALOGINSTUB",
                        "secretAccessKey": "loginsecret",
                        "sessionToken": "logintoken",
                    },
                    "tokenType": "aws_sigv4",
                    "expiresIn": 900,
                    "refreshToken": "login-refresh",
                    "idToken": "eyJhbGciOiJub25lIn0."  # gitleaks:allow
                    "eyJzdWIiOiAiYXJuOmF3czpzaWduaW46dXMtZWFzdC0xOjEyMzQ1Njc4OTAxMjpsb2dpbi1zZXNzaW9uL3N0dWIifQ.sig",
                },
            )
        self._send(404, {"message": "not found"})


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
