//! The `ec2` protocol: `EC2Serializer` (`botocore/serialize.py`) and
//! `EC2QueryParser` (`botocore/parsers.py`).

use anyhow::Result;

use crate::model::{Operation, ServiceModel};
use crate::protocol::query::{Variant, serialize_variant};
use crate::protocol::xml_common::{self, Flavor, XmlParser};
use crate::protocol::{HttpRequest, HttpResponse, ParseOptions, ServiceError};
use crate::value::Value;

/// `EC2Serializer.serialize_to_request`.
pub fn serialize(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
) -> Result<HttpRequest> {
    serialize_variant(model, operation, params, Variant::Ec2)
}

/// `EC2QueryParser.parse` (plus the endpoint's modeled error fields and
/// `RetryAttempts`).
pub fn parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<Value> {
    let parser = XmlParser {
        flavor: Flavor::Ec2,
        model,
        options: *options,
    };
    parser.parse(operation.output(), response, false)
}

/// `EC2QueryParser` error parsing (`_do_error_parse` and the modeled
/// error fields).
pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    let parser = XmlParser {
        flavor: Flavor::Ec2,
        model,
        options: *options,
    };
    Ok(xml_common::service_error(parser.parse(
        operation.output(),
        response,
        true,
    )?))
}

#[cfg(test)]
mod tests {
    //! Goldens produced by botocore (AWS CLI 2.39.3).

    use super::*;
    use crate::protocol::xml_common::test_support::*;

    #[test]
    fn golden_ser_describe_instances() {
        check_serialize(
            serialize,
            "ec2",
            "DescribeInstances",
            r#"{"InstanceIds": ["i-1", "i-2"], "Filters": [{"Name": "tag:Name", "Values": ["a b", "c"]}], "MaxResults": 5, "DryRun": false}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=DescribeInstances&Version=2016-11-15&InstanceId.1=i-1&InstanceId.2=i-2&Filter.1.Name=tag%3AName&Filter.1.Value.1=a+b&Filter.1.Value.2=c&MaxResults=5&DryRun=false","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_describe_instances_empty_list() {
        check_serialize(
            serialize,
            "ec2",
            "DescribeInstances",
            r#"{"InstanceIds": []}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=DescribeInstances&Version=2016-11-15","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_run_instances() {
        check_serialize(
            serialize,
            "ec2",
            "RunInstances",
            r#"{"ImageId": "ami-1", "MinCount": 1, "MaxCount": 1, "InstanceType": "t3.micro", "SecurityGroupIds": ["sg-1"], "UserData": "IyEvYmluL2Jhc2g=", "BlockDeviceMappings": [{"DeviceName": "/dev/xvda", "Ebs": {"VolumeSize": 8, "DeleteOnTermination": true}}], "TagSpecifications": [{"ResourceType": "instance", "Tags": [{"Key": "Name", "Value": "web"}]}]}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=RunInstances&Version=2016-11-15&ImageId=ami-1&MinCount=1&MaxCount=1&InstanceType=t3.micro&SecurityGroupId.1=sg-1&UserData=IyEvYmluL2Jhc2g%3D&BlockDeviceMapping.1.DeviceName=%2Fdev%2Fxvda&BlockDeviceMapping.1.Ebs.VolumeSize=8&BlockDeviceMapping.1.Ebs.DeleteOnTermination=true&TagSpecification.1.ResourceType=instance&TagSpecification.1.Tag.1.Key=Name&TagSpecification.1.Tag.1.Value=web","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_create_tags() {
        check_serialize(
            serialize,
            "ec2",
            "CreateTags",
            r#"{"Resources": ["i-1", "vol-2"], "Tags": [{"Key": "k", "Value": ""}]}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=CreateTags&Version=2016-11-15&ResourceId.1=i-1&ResourceId.2=vol-2&Tag.1.Key=k&Tag.1.Value=","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_modify_instance_attribute() {
        check_serialize(
            serialize,
            "ec2",
            "ModifyInstanceAttribute",
            r#"{"InstanceId": "i-1", "SourceDestCheck": {"Value": false}, "Groups": ["sg-1"]}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=ModifyInstanceAttribute&Version=2016-11-15&InstanceId=i-1&SourceDestCheck.Value=false&GroupId.1=sg-1","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_parse_describe_instances() {
        check_parse(
            parse,
            "ec2",
            "DescribeInstances",
            200,
            r#"[["Content-Type", "text/xml;charset=UTF-8"]]"#,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
    <requestId>8f7724cf-496f-496e-8fe3-example</requestId>
    <reservationSet>
        <item>
            <reservationId>r-1</reservationId>
            <ownerId>123</ownerId>
            <groupSet/>
            <instancesSet>
                <item>
                    <instanceId>i-1</instanceId>
                    <instanceState><code>16</code><name>running</name></instanceState>
                    <amiLaunchIndex>0</amiLaunchIndex>
                    <ebsOptimized>false</ebsOptimized>
                    <tagSet><item><key>Name</key><value>web</value></item></tagSet>
                </item>
            </instancesSet>
        </item>
    </reservationSet>
</DescribeInstancesResponse>"#,
            r#"{"Reservations":[{"ReservationId":"s:r-1","OwnerId":"s:123","Groups":[],"Instances":[{"EbsOptimized":false,"Tags":[{"Key":"s:Name","Value":"s:web"}],"InstanceId":"s:i-1","State":{"Code":"i:16","Name":"s:running"},"AmiLaunchIndex":"i:0"}]}],"ResponseMetadata":{"RequestId":"s:8f7724cf-496f-496e-8fe3-example","HTTPStatusCode":"i:200","HTTPHeaders":{"content-type":"s:text/xml;charset=UTF-8"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_describe_instances_ts() {
        check_parse(
            parse,
            "ec2",
            "DescribeInstances",
            200,
            r#"[]"#,
            r#"<DescribeInstancesResponse><requestId>r</requestId><reservationSet><item><instancesSet><item><instanceId>i-1</instanceId><launchTime>2026-01-02T03:04:05.000Z</launchTime></item></instancesSet></item></reservationSet></DescribeInstancesResponse>"#,
            r#"{"Reservations":[{"Instances":[{"InstanceId":"s:i-1","LaunchTime":"s:2026-01-02T03:04:05+00:00"}]}],"ResponseMetadata":{"RequestId":"s:r","HTTPStatusCode":"i:200","HTTPHeaders":{},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_create_tags() {
        check_parse(
            parse,
            "ec2",
            "CreateTags",
            200,
            r#"[]"#,
            r#"<CreateTagsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/"><requestId>r-1</requestId><return>true</return></CreateTagsResponse>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:r-1","HTTPStatusCode":"i:200","HTTPHeaders":{},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_run_instances() {
        check_parse(
            parse,
            "ec2",
            "RunInstances",
            200,
            r#"[]"#,
            r#"<RunInstancesResponse><requestId>r</requestId><reservationId>r-1</reservationId><ownerId>1</ownerId><groupSet><item><groupId>sg-1</groupId><groupName>default</groupName></item></groupSet><instancesSet><item><instanceId>i-1</instanceId><imageId>ami-1</imageId><instanceState><code>0</code><name>pending</name></instanceState><cpuOptions><coreCount>1</coreCount><threadsPerCore>2</threadsPerCore></cpuOptions></item></instancesSet></RunInstancesResponse>"#,
            r#"{"ReservationId":"s:r-1","OwnerId":"s:1","Groups":[{"GroupId":"s:sg-1","GroupName":"s:default"}],"Instances":[{"CpuOptions":{"CoreCount":"i:1","ThreadsPerCore":"i:2"},"InstanceId":"s:i-1","ImageId":"s:ami-1","State":{"Code":"i:0","Name":"s:pending"}}],"ResponseMetadata":{"RequestId":"s:r","HTTPStatusCode":"i:200","HTTPHeaders":{},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_error() {
        check_parse_error(
            parse_error,
            "ec2",
            "DescribeInstances",
            400,
            r#"[["Transfer-Encoding", "chunked"]]"#,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Response><Errors><Error><Code>InvalidInstanceID.NotFound</Code><Message>The instance ID 'i-1' does not exist</Message></Error></Errors><RequestID>a1b2</RequestID></Response>"#,
            r#"{"Error":{"Code":"s:InvalidInstanceID.NotFound","Message":"s:The instance ID 'i-1' does not exist"},"ResponseMetadata":{"RequestId":"s:a1b2","HTTPStatusCode":"i:400","HTTPHeaders":{"transfer-encoding":"s:chunked"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_error_generic() {
        check_parse_error(
            parse_error,
            "ec2",
            "DescribeInstances",
            502,
            r#"[]"#,
            r#"<html><body>Bad</body></html>"#,
            r#"{"Error":{"Code":"s:502","Message":"s:Bad Gateway"},"ResponseMetadata":{"HTTPStatusCode":"i:502","HTTPHeaders":{},"RetryAttempts":"i:0"}}"#,
        );
    }
}
