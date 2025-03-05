import {
  CfnOutput,
  Duration,
  Fn,
  SecretValue,
  Stack,
  StackProps,
} from "aws-cdk-lib";
import { SecurityGroup, Vpc } from "aws-cdk-lib/aws-ec2";
import { Effect, PolicyStatement } from "aws-cdk-lib/aws-iam";
import { Stream, StreamMode } from "aws-cdk-lib/aws-kinesis";
import {
  Architecture,
  LoggingFormat,
  StartingPosition,
} from "aws-cdk-lib/aws-lambda";
import { KinesisEventSource } from "aws-cdk-lib/aws-lambda-event-sources";
import { Secret } from "aws-cdk-lib/aws-secretsmanager";
import { RustFunction } from "cargo-lambda-cdk";
import { Construct } from "constructs";
import { join } from "path";

// Constants
const OTEL_EXPORTER_OTLP_PROTOCOL = "http/protobuf";
const OTEL_EXPORTER_OTLP_COMPRESSION = "gzip";

export interface MyStackProps extends StackProps {
  collectorsSecretsKeyPrefix?: string;
  collectorsCacheTtlSeconds?: number;
  vpcId?: string;
  subnetIds?: string[];
  kinesisStreamMode?: string;
  shardCount?: number;
  otelExporterOtlpEndpoint?: string;
  otelExporterOtlpHeaders?: string;
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
    const otelExporterOtlpEndpoint =
      props.otelExporterOtlpEndpoint || "https://example.com/v1/traces";
    const otelExporterOtlpHeaders =
      props.otelExporterOtlpHeaders || "api-key=your-api-key";

    // Check conditions (similar to the SAM template)
    const isProvisionedMode = kinesisStreamMode === "PROVISIONED";
    const hasVpcConfig = vpcId !== "";

    //==============================================================================
    // SECRETS MANAGER
    //==============================================================================

    // Create the collector secret
    const collectorSecret = new Secret(this, "DefaultCollectorSecret", {
      secretName: `${collectorsSecretsKeyPrefix}/default`,
      description: "Default OTLP collector configuration",
      secretStringValue: SecretValue.unsafePlainText(
        JSON.stringify({
          name: "default",
          endpoint: otelExporterOtlpEndpoint,
          auth: otelExporterOtlpHeaders,
        }),
      ),
    });

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
        manifestPath: join(
          __dirname,
          "..",
          "functions/processor",
          "Cargo.toml",
        ),
        bundling: { cargoLambdaFlags: ["--quiet"] },
        architecture: Architecture.ARM_64,
        timeout: Duration.seconds(60),
        loggingFormat: LoggingFormat.JSON,
        environment: {
          RUST_LOG: "info",
          OTEL_SERVICE_NAME: this.stackName,
          OTEL_EXPORTER_OTLP_ENDPOINT: otelExporterOtlpEndpoint,
          OTEL_EXPORTER_OTLP_HEADERS: otelExporterOtlpHeaders,
          OTEL_EXPORTER_OTLP_PROTOCOL: OTEL_EXPORTER_OTLP_PROTOCOL,
          OTEL_EXPORTER_OTLP_COMPRESSION: OTEL_EXPORTER_OTLP_COMPRESSION,
          COLLECTORS_CACHE_TTL_SECONDS: collectorsCacheTtlSeconds.toString(),
          COLLECTORS_SECRETS_KEY_PREFIX: `${collectorsSecretsKeyPrefix}/`,
        },
        vpc: hasVpcConfig ? vpc : undefined,
        securityGroups: hasVpcConfig ? [securityGroup!] : undefined,
      },
    );

    // Grant the Lambda function permission to read the secret
    collectorSecret.grantRead(kinesisProcessorFunction);

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

    new CfnOutput(this, "OtlpKinesisStreamName", {
      description: "Name of the Kinesis stream for OTLP data",
      value: otlpKinesisStream.streamName,
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
