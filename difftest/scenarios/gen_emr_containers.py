"""Generate emr-containers.json (the scenarios of the emr-containers commands)."""
import json

J = {"Content-Type": "application/json"}
X = {"Content-Type": "text/xml"}
CONFIG = "[default]\nregion = us-east-1\nendpoint_url = {endpoint}\n"


def j(body, status=200):
    return {"status": status, "headers": J, "body": json.dumps(body)}


def ixml(op, inner=""):
    return {"status": 200, "headers": X, "body": f'<{op}Response xmlns="https://iam.amazonaws.com/doc/2010-05-08/"><{op}Result>{inner}</{op}Result><ResponseMetadata><RequestId>r</RequestId></ResponseMetadata></{op}Response>'}


def sc(name, argv, responses, config=CONFIG):
    return {"name": name, "argv": ["emr-containers"] + argv, "responses": responses, "files": {"home/.aws/config": config}}


CL = j({"cluster": {"name": "c", "arn": "arn:aws:eks:us-east-1:123456789012:cluster/c",
                    "identity": {"oidc": {"issuer": "https://oidc.eks.us-east-1.amazonaws.com/id/ABC"}}}})
OIDC = "oidc.eks.us-east-1.amazonaws.com/id/ABC"
existing = {"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"Service": "ec2.amazonaws.com"}, "Action": "sts:AssumeRole"}]}
matching = {"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"Federated": f"arn:aws:iam::123456789012:oidc-provider/{OIDC}"},
            "Action": "sts:AssumeRoleWithWebIdentity", "Condition": {"StringLike": {f"{OIDC}:sub": "system:serviceaccount:ns:emr-containers-sa-*-*-123456789012-u70qubg4l"}}}]}


def role(doc):
    from urllib.parse import quote
    return ixml("GetRole", f"<Role><Path>/</Path><RoleName>MyRole</RoleName><RoleId>A</RoleId><Arn>arn:aws:iam::1:role/MyRole</Arn><CreateDate>2020-01-01T00:00:00Z</CreateDate><AssumeRolePolicyDocument>{quote(json.dumps(doc))}</AssumeRolePolicyDocument></Role>")


BASE = ["--cluster-name", "c", "--namespace", "ns", "--role-name", "MyRole"]
S = [
    sc("emr-containers update-role-trust-policy", ["update-role-trust-policy"] + BASE, [CL, role(existing), ixml("UpdateAssumeRolePolicy")]),
    sc("emr-containers update-role-trust-policy dry run", ["update-role-trust-policy"] + BASE + ["--dry-run"], [CL, role(existing)]),
    sc("emr-containers update-role-trust-policy exists", ["update-role-trust-policy"] + BASE, [CL, role(matching)]),
    sc("emr-containers update-role-trust-policy iam endpoint", ["update-role-trust-policy"] + BASE + ["--iam-endpoint", "{endpoint}", "--dry-run"], [CL, role({"Version": "1"})]),
    sc("emr-containers update-role-trust-policy missing", ["update-role-trust-policy", "--cluster-name", "c"], [CL]),
    sc("emr-containers update-role-trust-policy eks error", ["update-role-trust-policy"] + BASE,
       [{"status": 404, "headers": {"Content-Type": "application/json", "x-amzn-ErrorType": "ResourceNotFoundException"}, "body": '{"message":"no cluster"}'}]),
]
ASSOC = lambda sa: j({"association": {"clusterName": "c", "namespace": "ns", "serviceAccount": sa, "roleArn": "arn:aws:iam::123456789012:role/MyRole", "associationArn": "arn:x", "associationId": "a-" + sa[-3:]}})
INUSE = {"status": 409, "headers": {"Content-Type": "application/json", "x-amzn-ErrorType": "ResourceInUseException"}, "body": '{"message":"exists"}'}
BAD = {"status": 400, "headers": {"Content-Type": "application/json", "x-amzn-ErrorType": "InvalidParameterException"}, "body": '{"message":"bad"}'}
for t in ["start_job_run", "interactive_endpoint", "spark_operator", "flink_operator", "livy"]:
    S.append(sc("emr-containers create-role-associations " + t, ["create-role-associations"] + BASE + ["--type", t, "--operator-namespace", "op"],
                [CL, ASSOC("one"), ASSOC("two"), ASSOC("three")]))
S.append(sc("emr-containers create-role-associations service account", ["create-role-associations"] + BASE + ["--service-account-name", "sa"], [CL, ASSOC("sa")]))
S.append(sc("emr-containers create-role-associations in use", ["create-role-associations"] + BASE, [CL, ASSOC("one"), INUSE, ASSOC("three")]))
S.append(sc("emr-containers create-role-associations rollback", ["create-role-associations"] + BASE, [CL, ASSOC("one"), BAD, j({})]))
S.append(sc("emr-containers create-role-associations timestamp", ["create-role-associations"] + BASE + ["--service-account-name", "sa"],
            [CL, j({"association": {"clusterName": "c", "createdAt": 1700000000}})]))
S.append(sc("emr-containers create-role-associations bad type", ["create-role-associations"] + BASE + ["--type", "foo"], [CL]))
LIST = lambda: j({"associations": [{"associationId": "a-1", "clusterName": "c"}]})
S.append(sc("emr-containers delete-role-associations", ["delete-role-associations"] + BASE,
            [CL, LIST(), ASSOC("one"), j({"associations": []}), LIST(), ASSOC("three")]))
S.append(sc("emr-containers delete-role-associations none", ["delete-role-associations"] + BASE + ["--type", "livy"], [CL, j({"associations": []})]))
S.append(sc("emr-containers delete-role-associations error", ["delete-role-associations"] + BASE, [CL, BAD]))
print(json.dumps(S, indent=1))
