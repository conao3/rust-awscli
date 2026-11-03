import json
J={"Content-Type":"application/x-amz-json-1.1"}
X={"Content-Type":"text/xml"}
def j(body,status=200): return {"status":status,"headers":J,"body":json.dumps(body)}
def err(code,msg="bad",status=400): return {"status":status,"headers":J,"body":json.dumps({"__type":code,"message":msg})}
EP=["--endpoint-url","{endpoint}"]
def sc(name,argv,responses=None,files=None,outputs=None):
    d={"name":name,"argv":argv}
    if responses: d["responses"]=responses
    if files: d["files"]=files
    if outputs: d["outputs"]=outputs
    return d
S=[]
cl=lambda **kw: {"Cluster":dict({"Id":"j-1","Name":"c","Status":{"State":"RUNNING"}},**kw)}
S.append(sc("emr terminate-clusters",["emr","terminate-clusters","--cluster-ids","j-1","j-2"]+EP,[j({})]))
S.append(sc("emr terminate-clusters missing",["emr","terminate-clusters"]+EP))
S.append(sc("emr describe-cluster groups",["emr","describe-cluster","--cluster-id","j-1"]+EP,
  [j(cl(ReleaseLabel="emr-6.0.0")),j({"InstanceGroups":[{"Id":"ig-1","InstanceGroupType":"MASTER"}]}),j({"BootstrapActions":[{"Name":"b","ScriptPath":"s3://x"}]})]))
S.append(sc("emr describe-cluster fleets table",["emr","describe-cluster","--cluster-id","j-1","--output","table"]+EP,
  [j(cl(InstanceCollectionType="INSTANCE_FLEET")),j({"InstanceFleets":[{"Id":"if-1"}]}),j({})]))
S.append(sc("emr describe-cluster text",["emr","describe-cluster","--cluster-id","j-1","--output","text"]+EP,
  [j(cl()),j({"InstanceGroups":[]}),j({"BootstrapActions":[]})]))
S.append(sc("emr describe-cluster service error",["emr","describe-cluster","--cluster-id","j-1"]+EP,[err("InvalidRequestException","no cluster")]))
S.append(sc("emr modify-cluster-attributes",["emr","modify-cluster-attributes","--cluster-id","j-1","--visible-to-all-users","--no-termination-protected","--auto-terminate","--unhealthy-node-replacement"]+EP,[j({})]))
S.append(sc("emr modify-cluster-attributes none",["emr","modify-cluster-attributes","--cluster-id","j-1"]+EP))
S.append(sc("emr modify-cluster-attributes both",["emr","modify-cluster-attributes","--cluster-id","j-1","--auto-terminate","--no-auto-terminate"]+EP))
S.append(sc("emr add-steps release",["emr","add-steps","--cluster-id","j-1","--steps",
  "Type=Spark,Name=s,Args=[--class,x,s3://a.jar]","Type=CUSTOM_JAR,Jar=s3://j.jar,Args=[a],MainClass=M,Properties=k=v,ActionOnFailure=CONTINUE",
  "Type=Hive,Args=[-f,s3://q]","Type=Pig,Args=[-f,x]","Type=Streaming,Args=[-mapper,m]","--execution-role-arn","arn:aws:iam::123456789012:role/r"]+EP,
  [j(cl(ReleaseLabel="emr-6.0.0")),j({"StepIds":["s-1","s-2"]})]))
S.append(sc("emr add-steps ami",["emr","add-steps","--cluster-id","j-1","--steps","Type=Hive,Args=[-f,s3://q]","Type=Pig,Args=[-f,x]","Type=Streaming,Args=[-mapper,m]","Type=Impala,Args=[a]","Type=Spark,Args=[x]"]+EP,
  [j(cl()),j({"StepIds":["s-1"]})]))
S.append(sc("emr add-steps unknown type",["emr","add-steps","--cluster-id","j-1","--steps","Type=Foo,Args=[a]"]+EP,[j(cl())]))
S.append(sc("emr add-steps missing jar",["emr","add-steps","--cluster-id","j-1","--steps","Name=x"]+EP,[j(cl())]))
S.append(sc("emr add-steps empty args",["emr","add-steps","--cluster-id","j-1","--steps","Type=Hive,Args=[]"]+EP,[j(cl())]))
S.append(sc("emr add-instance-groups",["emr","add-instance-groups","--cluster-id","j-1","--instance-groups",
  "InstanceGroupType=TASK,InstanceType=m5.xlarge,InstanceCount=2,BidPrice=0.1","InstanceGroupType=task,InstanceType=m5.xlarge,InstanceCount=1,Name=n,BidPrice=OnDemandPrice"]+EP,
  [j({"JobFlowId":"j-1","InstanceGroupIds":["ig-1"],"ClusterArn":"arn:c"})]))
S.append(sc("emr add-instance-groups missing type",["emr","add-instance-groups","--cluster-id","j-1","--instance-groups","InstanceType=m5.xlarge,InstanceCount=2"]+EP))
S.append(sc("emr install-applications",["emr","install-applications","--cluster-id","j-1","--applications","Name=Hive,Args=[--hive-site=s3://h]","Name=Pig"]+EP,
  [j(cl()),j({})]))
S.append(sc("emr install-applications release",["emr","install-applications","--cluster-id","j-1","--applications","Name=Hive"]+EP,[j(cl(ReleaseLabel="emr-5.0.0"))]))
S.append(sc("emr install-applications unsupported",["emr","install-applications","--cluster-id","j-1","--applications","Name=HBase"]+EP,[j(cl())]))
S.append(sc("emr install-applications unknown",["emr","install-applications","--cluster-id","j-1","--applications","Name=Foo"]+EP,[j(cl())]))
for name,args in [("restore-from-hbase-backup",["--dir","s3://b","--backup-version","v1"]),("create-hbase-backup",["--dir","s3://b","--consistent"]),
  ("schedule-hbase-backup",["--type","FULL","--dir","s3://b","--interval","3","--unit","Hours","--consistent"]),
  ("schedule-hbase-backup",["--type","incremental","--dir","s3://b","--interval","3","--unit","days","--start-time","2014-04-21T05:26:10Z"]),
  ("disable-hbase-backups",["--full","--incremental"])]:
    S.append(sc("emr "+name+" "+" ".join(args),["emr",name,"--cluster-id","j-1"]+args+EP,[j(cl()),j({"StepIds":["s-1"]})]))
S.append(sc("emr schedule-hbase-backup bad type",["emr","schedule-hbase-backup","--cluster-id","j-1","--type","x","--dir","d","--interval","1","--unit","days"]+EP,[j(cl())]))
S.append(sc("emr schedule-hbase-backup bad unit",["emr","schedule-hbase-backup","--cluster-id","j-1","--type","full","--dir","d","--interval","1","--unit","weeks"]+EP,[j(cl())]))
S.append(sc("emr disable-hbase-backups none",["emr","disable-hbase-backups","--cluster-id","j-1"]+EP,[j(cl())]))
RJ=j({"JobFlowId":"j-NEW","ClusterArn":"arn:aws:elasticmapreduce:us-east-1:1:cluster/j-NEW"})
S.append(sc("emr create-cluster release",["emr","create-cluster","--release-label","emr-6.10.0","--instance-type","m5.xlarge","--instance-count","3",
  "--applications","Name=Spark","Name=Hive","--tags","a=b","c","--ec2-attributes","KeyName=k,SubnetId=s,EmrManagedMasterSecurityGroup=sg",
  "--use-default-roles","--log-uri","s3://logs","--enable-debugging","--steps","Type=Hive,Args=[-f,q]","Type=CUSTOM_JAR,Jar=j.jar",
  "--name","my","--auto-terminate","--termination-protected","--security-configuration","sc","--ebs-root-volume-size","20",
  "--step-concurrency-level","5","--emrfs","Consistent=true,RetryCount=3,SSE=true","--bootstrap-actions","Path=s3://ba,Args=[x]","Name=n,Path=s3://b2",
  "--managed-scaling-policy","ComputeLimits={UnitType=Instances,MinimumCapacityUnits=1,MaximumCapacityUnits=3}","--extended-support","--no-session-enabled"]+EP,[RJ]))
S.append(sc("emr create-cluster ami",["emr","create-cluster","--ami-version","3.11.0","--instance-groups",
  "InstanceGroupType=MASTER,InstanceType=m3.xlarge,InstanceCount=1","InstanceGroupType=CORE,InstanceType=m3.xlarge,InstanceCount=2,BidPrice=0.2",
  "--applications","Name=Hive,Args=[--hive-site=s3://h]","Name=Pig","Name=HBase","Name=Ganglia","Name=Impala,Args=[a,b]","Name=MapR,Args=[--edition,m5]",
  "--restore-from-hbase-backup","Dir=s3://hb,BackupVersion=v","--emrfs","Encryption=ClientSide,ProviderType=Custom,CustomProviderLocation=s3://p,CustomProviderClass=C,Args=[a=b,c]",
  "--service-role","sr","--ec2-attributes","InstanceProfile=ip,AvailabilityZone=us-east-1a","--no-visible-to-all-users","--enable-debugging","--log-uri","s3://l"]+EP,[RJ]))
S.append(sc("emr create-cluster fleets json",["emr","create-cluster","--release-label","emr-6.0.0","--instance-fleets",
  '[{"InstanceFleetType":"MASTER","TargetOnDemandCapacity":1,"InstanceTypeConfigs":[{"InstanceType":"m5.xlarge"}],"LaunchSpecifications":{"OnDemandSpecification":{"AllocationStrategy":"lowest-price"}}}]',
  "--configurations","file://conf.json","--emrfs","Encryption=ClientSide,ProviderType=KMS,KMSKeyId=k","--output","yaml"]+EP,[RJ],
  files={"conf.json":'[{"Classification":"spark","Properties":{"a":"b"}}]'}))
for nm,extra in [("no release",["--instance-type","m5.xlarge"]),("both release ami",["--release-label","x","--ami-version","3.0","--instance-type","m"]),
  ("bad ami",["--ami-version","abc","--instance-type","m"]),("no instances",["--release-label","emr-6.0.0"]),
  ("groups and type",["--release-label","e","--instance-type","m","--instance-groups","InstanceGroupType=MASTER,InstanceType=m,InstanceCount=1"]),
  ("default roles and service role",["--release-label","e","--instance-type","m","--use-default-roles","--service-role","x"]),
  ("default roles and instance profile",["--release-label","e","--instance-type","m","--use-default-roles","--ec2-attributes","InstanceProfile=x"]),
  ("debugging no log",["--release-label","e","--instance-type","m","--enable-debugging"]),
  ("auto terminate both",["--release-label","e","--instance-type","m","--auto-terminate","--no-auto-terminate"]),
  ("subnet and az",["--release-label","e","--instance-type","m","--ec2-attributes","SubnetId=s,AvailabilityZone=a"]),
  ("missing apps",["--release-label","e","--instance-type","m","--steps","Type=Hive,Args=[a]","Type=PIG,Args=[b]"]),
  ("bad configurations",["--release-label","e","--instance-type","m","--configurations","notjson"]),
  ("bad instance count",["--release-label","e","--instance-type","m","--instance-count","x"]),
  ("emrfs both",["--release-label","e","--instance-type","m","--emrfs","SSE=true,Encryption=ServerSide"]),
  ("emrfs bad encryption",["--release-label","e","--instance-type","m","--emrfs","Encryption=foo"]),
  ("emrfs kms missing",["--release-label","e","--instance-type","m","--emrfs","Encryption=ClientSide,ProviderType=KMS"]),
  ("emrfs retry without consistent",["--release-label","e","--instance-type","m","--emrfs","RetryCount=3"]),
  ("autoscaling without role",["--release-label","e","--instance-groups",'InstanceGroupType=MASTER,InstanceType=m,InstanceCount=1,AutoScalingPolicy={Constraints={MinCapacity=1,MaxCapacity=2},Rules=[]}']),
  ("monitoring logs",["--release-label","e","--instance-type","m","--monitoring-configuration",'S3LoggingConfiguration={LogTypeUploadPolicy={system-logs=emr-managed}}']),
  ("unknown option",["--release-label","e","--instance-type","m","--relese"]),
  ]:
    S.append(sc("emr create-cluster error "+nm,["emr","create-cluster"]+extra+EP,[RJ]))
S.append(sc("emr list-clusters active",["emr","list-clusters","--active"]+EP,[j({"Clusters":[{"Id":"j-1","Name":"a"}]})]))
S.append(sc("emr list-clusters states created",["emr","list-clusters","--cluster-states","RUNNING","WAITING","--created-after","2020-01-01T00:00:00Z","--created-before","2021-01-01"]+EP,[j({"Clusters":[]})]))
S.append(sc("emr list-clusters two filters",["emr","list-clusters","--active","--failed"]+EP))
S.append(sc("emr list-clusters terminated pages",["emr","list-clusters","--terminated","--max-items","1"]+EP,[j({"Clusters":[{"Id":"j-1"},{"Id":"j-2"}],"Marker":"m"})]))
S.append(sc("emr add-tags",["emr","add-tags","--resource-id","j-1","--tags","a=b","c","d=e=f"]+EP,[j({})]))
S.append(sc("emr add-tags missing tags",["emr","add-tags","--resource-id","j-1"]+EP))
def ixml(op,inner=""):
    return {"status":200,"headers":X,"body":f'<{op}Response xmlns="https://iam.amazonaws.com/doc/2010-05-08/"><{op}Result>{inner}</{op}Result><ResponseMetadata><RequestId>r</RequestId></ResponseMetadata></{op}Response>'}
nse={"status":404,"headers":X,"body":'<ErrorResponse xmlns="https://iam.amazonaws.com/doc/2010-05-08/"><Error><Type>Sender</Type><Code>NoSuchEntity</Code><Message>nope</Message></Error><RequestId>r</RequestId></ErrorResponse>'}
role='<Role><Path>/</Path><RoleName>EMR_EC2_DefaultRole</RoleName><RoleId>AROA</RoleId><Arn>arn:aws:iam::1:role/EMR_EC2_DefaultRole</Arn><CreateDate>2020-01-01T00:00:00Z</CreateDate><AssumeRolePolicyDocument>%7B%22Version%22%3A%222008-10-17%22%7D</AssumeRolePolicyDocument></Role>'
EXIST=[ixml("GetRole",role),ixml("GetInstanceProfile","<InstanceProfile><InstanceProfileName>x</InstanceProfileName></InstanceProfile>"),ixml("GetRole",role),ixml("GetRole",role)]
S.append(sc("emr create-default-roles all exist",["emr","create-default-roles","--iam-endpoint","{endpoint}"],EXIST,outputs=["home/.aws/config"]))
S.append(sc("emr create-default-roles create ec2",["emr","create-default-roles","--iam-endpoint","{endpoint}"],
  [nse,ixml("CreateRole",role),ixml("AttachRolePolicy"),ixml("GetPolicy","<Policy><DefaultVersionId>v2</DefaultVersionId></Policy>"),
   ixml("GetPolicyVersion","<PolicyVersion><Document>%7B%22Statement%22%3A%5B%5D%7D</Document><VersionId>v2</VersionId></PolicyVersion>"),
   nse,ixml("CreateInstanceProfile","<InstanceProfile><InstanceProfileName>x</InstanceProfileName></InstanceProfile>"),ixml("AddRoleToInstanceProfile"),ixml("GetRole",role)],outputs=["home/.aws/config"]))
S.append(sc("emr create-default-roles iam error",["emr","create-default-roles","--iam-endpoint","{endpoint}"],
  [{"status":403,"headers":X,"body":'<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code><Message>no</Message></Error><RequestId>r</RequestId></ErrorResponse>'}]))
S.append(sc("emr create-default-roles create error",["emr","create-default-roles","--iam-endpoint","{endpoint}"]+EP,[nse]))
S.append(sc("emr create-default-roles principal error",["emr","create-default-roles","--iam-endpoint","{endpoint}"]+EP,EXIST[:2]+[nse]))
S.append(sc("emr create-default-roles create all",["emr","create-default-roles","--iam-endpoint","{endpoint}","--region","eu-west-1","--output","text"],
  [nse,ixml("CreateRole",role),ixml("AttachRolePolicy"),ixml("GetPolicy","<Policy><DefaultVersionId>v2</DefaultVersionId></Policy>"),
   ixml("GetPolicyVersion","<PolicyVersion><Document>%7B%22Statement%22%3A%5B%5D%7D</Document><VersionId>v2</VersionId></PolicyVersion>"),
   ixml("GetInstanceProfile","<InstanceProfile><InstanceProfileName>x</InstanceProfileName></InstanceProfile>"),
   nse,ixml("CreateRole",role),ixml("AttachRolePolicy"),ixml("GetPolicy","<Policy><DefaultVersionId>v1</DefaultVersionId></Policy>"),
   ixml("GetPolicyVersion","<PolicyVersion><Document>%7B%7D</Document><VersionId>v1</VersionId></PolicyVersion>"),
   nse,ixml("CreateRole",role),ixml("AttachRolePolicy"),ixml("GetPolicy","<Policy><DefaultVersionId>v1</DefaultVersionId></Policy>"),
   ixml("GetPolicyVersion","<PolicyVersion><Document>%7B%7D</Document><VersionId>v1</VersionId></PolicyVersion>")],
  files={"home/.aws/config":"[default]\nregion = us-east-1\n[profile other]\nregion = x\n"},outputs=["home/.aws/config"]))
S.append(sc("emr create-default-roles configured",["emr","create-default-roles","--iam-endpoint","{endpoint}"],EXIST,
  files={"home/.aws/config":"[default]\nregion = us-east-1\nemr =\n    service_role = X\n"},outputs=["home/.aws/config"]))
S.append(sc("emr create-cluster from config",["emr","create-cluster","--release-label","e","--instance-type","m"]+EP,[RJ],
  files={"home/.aws/config":"[default]\nregion = us-east-1\nemr =\n    service_role = SR\n    instance_profile = IP\n    key_name = KN\n    log_uri = s3://cfg\n    enable_debugging = True\n"}))
S.append(sc("emr create-cluster bad bool config",["emr","create-cluster","--release-label","e","--instance-type","m"]+EP,[RJ],
  files={"home/.aws/config":"[default]\nregion = us-east-1\nemr =\n    enable_debugging = maybe\n"}))
DNS=cl(MasterPublicDnsName="master.invalid")
S.append(sc("emr ssh",["emr","ssh","--cluster-id","j-1","--key-pair-file","k.pem","--command","ls"]+EP,[j(DNS)]))
S.append(sc("emr ssh options",["emr","ssh","--cluster-id","j-1","--key-pair-file","k.pem","--ssh-options","StrictHostKeyChecking=no","ConnectTimeout=1"]+EP,[j(DNS)]))
S.append(sc("emr ssh terminated",["emr","ssh","--cluster-id","j-1","--key-pair-file","k.pem"]+EP,[j({"Cluster":{"Status":{"State":"TERMINATED"}}})]))
S.append(sc("emr ssh key from config",["emr","ssh","--cluster-id","j-1"]+EP,[j(DNS)],files={"home/.aws/config":"[default]\nregion = us-east-1\nemr =\n    key_pair_file = cfg.pem\n"}))
S.append(sc("emr socks",["emr","socks","--cluster-id","j-1","--key-pair-file","k.pem"]+EP,[j(DNS)]))
S.append(sc("emr put",["emr","put","--cluster-id","j-1","--key-pair-file","k.pem","--src","dir/f.txt"]+EP,[j(DNS)]))
S.append(sc("emr get",["emr","get","--cluster-id","j-1","--key-pair-file","k.pem","--src","/tmp/x/f.txt","--dest","out"]+EP,[j(DNS)]))
S.append(sc("emr ssh missing key",["emr","ssh","--cluster-id","j-1"]+EP))
print(json.dumps(S,indent=1))
