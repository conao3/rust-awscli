import http.server
import sys
import urllib.parse

STS = b'<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><GetCallerIdentityResult><Arn>arn:aws:iam::123456789012:user/bench</Arn><UserId>AIDAEXAMPLE</UserId><Account>123456789012</Account></GetCallerIdentityResult><ResponseMetadata><RequestId>r</RequestId></ResponseMetadata></GetCallerIdentityResponse>'
EC2 = b'<DescribeRegionsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/"><requestId>r</requestId><regionInfo><item><regionName>us-east-1</regionName><regionEndpoint>ec2.us-east-1.amazonaws.com</regionEndpoint><optInStatus>opt-in-not-required</optInStatus></item><item><regionName>eu-west-1</regionName><regionEndpoint>ec2.eu-west-1.amazonaws.com</regionEndpoint><optInStatus>opt-in-not-required</optInStatus></item></regionInfo></DescribeRegionsResponse>'
DYNAMODB = b'{"TableNames":["orders","users"]}'


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        if self.headers.get("x-amz-target", "").startswith("DynamoDB"):
            payload, content_type = DYNAMODB, "application/x-amz-json-1.0"
        elif urllib.parse.parse_qs(body.decode()).get("Action") == ["DescribeRegions"]:
            payload, content_type = EC2, "text/xml"
        else:
            payload, content_type = STS, "text/xml"
        self.send_response(200)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
