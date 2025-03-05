import { CfnOutput, Duration, Fn, Stack, StackProps } from "aws-cdk-lib";
import { SecurityGroup, Vpc } from "aws-cdk-lib/aws-ec2";
import { Effect, PolicyStatement } from "aws-cdk-lib/aws-iam";
import { Stream, StreamMode } from "aws-cdk-lib/aws-kinesis";
import { Architecture, StartingPosition } from "aws-cdk-lib/aws-lambda";
import { KinesisEventSource } from "aws-cdk-lib/aws-lambda-event-sources";
import { RustFunction } from "cargo-lambda-cdk";
import { Construct } from "constructs";
import { join } from "path";

export interface MyStackProps extends StackProps {
  collectorsSecretsKeyPrefix?: string;
  collectorsCacheTtlSeconds?: number;
  vpcId?: string;
  subnetIds?: string[];
  kinesisStreamMode?: string;
  shardCount?: number;
}

export class MyStack extends Stack {
  constructor(scope: Construct, id: string, props: MyStackProps = {}) {
    super(scope, id, props);

    // Set default values for parameters
    const collectorsSecretsKeyPrefix =
      props.collectorsSecretsKeyPrefix || "serverless-otlp-forwarder/keys";
    const collectorsCacheTtlSeconds = props.collectorsCacheTtlSeconds || 300;
    const vpcId = props.vpcId || "";
    const subnetIds = props.subnetIds || [];
    const kinesisStreamMode = props.kinesisStreamMode || "PROVISIONED";
    const shardCount = props.shardCount || 1;

    // Check conditions (similar to the SAM template)
    const isProvisionedMode = kinesisStreamMode === "PROVISIONED";
    const hasVpcConfig = vpcId !== "";

    // Create Kinesis Stream
    const otlpKinesisStream = new Stream(this, "OtlpKinesisStream", {
      streamName: `${this.stackName}-otlp-stream`,
      streamMode: isProvisionedMode
        ? StreamMode.PROVISIONED
        : StreamMode.ON_DEMAND,
      shardCount: isProvisionedMode ? shardCount : undefined,
      retentionPeriod: Duration.hours(24),
    });

    // Create Security Group if VPC config is provided
    let securityGroup;
    let vpc;

    if (hasVpcConfig) {
      vpc = Vpc.fromVpcAttributes(this, "ImportedVpc", {
        vpcId: vpcId,
        availabilityZones: Fn.getAzs(),
        privateSubnetIds: subnetIds,
      });

      securityGroup = new SecurityGroup(this, "KinesisProcessorSecurityGroup", {
        vpc,
        description: "Security group for OTLP Kinesis Processor Lambda",
        allowAllOutbound: true,
      });
    }

    // Create Lambda Function for processing Kinesis events
    const kinesisProcessorFunction = new RustFunction(
      this,
      "KinesisProcessorFunction",
      {
        functionName: this.stackName,
        description: `Processes OTLP data from Kinesis stream in AWS Account ${this.account}`,
        manifestPath: join(__dirname, "functions/processor", "Cargo.toml"),
        architecture: Architecture.ARM_64,
        timeout: Duration.seconds(60),
        environment: {
          RUST_LOG: "info",
          OTEL_SERVICE_NAME: this.stackName,
          OTEL_EXPORTER_OTLP_ENDPOINT: `{{resolve:secretsmanager:${collectorsSecretsKeyPrefix}/default:SecretString:endpoint}}`,
          OTEL_EXPORTER_OTLP_HEADERS: `{{resolve:secretsmanager:${collectorsSecretsKeyPrefix}/default:SecretString:auth}}`,
          OTEL_EXPORTER_OTLP_PROTOCOL: "http/protobuf",
          COLLECTORS_CACHE_TTL_SECONDS: collectorsCacheTtlSeconds.toString(),
          COLLECTORS_SECRETS_KEY_PREFIX: `${collectorsSecretsKeyPrefix}/`,
        },
        vpc: hasVpcConfig ? vpc : undefined,
        securityGroups: hasVpcConfig ? [securityGroup!] : undefined,
      },
    );

    // Add IAM policies to the Lambda function
    kinesisProcessorFunction.addToRolePolicy(
      new PolicyStatement({
        effect: Effect.ALLOW,
        actions: [
          "secretsmanager:BatchGetSecretValue",
          "secretsmanager:ListSecrets",
          "xray:PutTraceSegments",
          "xray:PutSpans",
          "xray:PutSpansForIndexing",
        ],
        resources: ["*"],
      }),
    );

    kinesisProcessorFunction.addToRolePolicy(
      new PolicyStatement({
        effect: Effect.ALLOW,
        actions: ["secretsmanager:GetSecretValue"],
        resources: [
          `arn:${this.partition}:secretsmanager:${this.region}:${this.account}:secret:${collectorsSecretsKeyPrefix}/*`,
        ],
      }),
    );

    kinesisProcessorFunction.addToRolePolicy(
      new PolicyStatement({
        effect: Effect.ALLOW,
        actions: [
          "kinesis:GetRecords",
          "kinesis:GetShardIterator",
          "kinesis:DescribeStream",
          "kinesis:ListShards",
        ],
        resources: [otlpKinesisStream.streamArn],
      }),
    );

    // Add Kinesis as an event source for the Lambda function
    kinesisProcessorFunction.addEventSource(
      new KinesisEventSource(otlpKinesisStream, {
        startingPosition: StartingPosition.LATEST,
        batchSize: 100,
        maxBatchingWindow: Duration.seconds(5),
        reportBatchItemFailures: true,
      }),
    );

    // Outputs
    new CfnOutput(this, "KinesisProcessorFunctionName", {
      description: "Name of the Kinesis processor Lambda function",
      value: kinesisProcessorFunction.functionName,
    });

    new CfnOutput(this, "KinesisProcessorFunctionArn", {
      description: "ARN of the Kinesis processor Lambda function",
      value: kinesisProcessorFunction.functionArn,
    });

    if (hasVpcConfig) {
      new CfnOutput(this, "KinesisProcessorSecurityGroupId", {
        description:
          "ID of the security group for the Kinesis processor Lambda function",
        value: securityGroup!.securityGroupId,
      });
    }
  }
}
