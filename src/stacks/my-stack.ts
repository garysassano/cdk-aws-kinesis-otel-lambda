import {
  CfnOutput,
  Duration,
  RemovalPolicy,
  Stack,
  StackProps,
} from "aws-cdk-lib";
import { Stream, StreamMode } from "aws-cdk-lib/aws-kinesis";
import {
  Architecture,
  FunctionUrlAuthType,
  LoggingFormat,
} from "aws-cdk-lib/aws-lambda";
import { RustExtension, RustFunction } from "cargo-lambda-cdk";
import { Construct } from "constructs";
import { join } from "path";
import {
  Role,
  PolicyStatement,
  PolicyDocument,
  ManagedPolicy,
  Effect,
  ArnPrincipal,
} from "aws-cdk-lib/aws-iam";

export class MyStack extends Stack {
  constructor(scope: Construct, id: string, props: StackProps = {}) {
    super(scope, id, props);

    //==============================================================================
    // KINESIS RESOURCES
    //==============================================================================

    // Create Kinesis Stream
    const otlpKinesisStream = new Stream(this, "OtlpKinesisStream", {
      streamName: `${id}-otlp-stream`,
      retentionPeriod: Duration.hours(24),
      streamMode: StreamMode.ON_DEMAND,
      removalPolicy: RemovalPolicy.DESTROY,
    });

    //==============================================================================
    // IAM ROLE FOR CLICKHOUSE ACCESS
    //==============================================================================

    // Create IAM role for ClickHouse access with trust policy
    const clickHouseAccessRole = new Role(this, "ClickHouseAccessRole", {
      roleName: `ClickHouseAccessRole-${id}`,
      assumedBy: new ArnPrincipal(
        "arn:aws:iam::426924874929:role/CH-S3-bisque-uh-92-uw2-83-Role",
      ),
      description: "Role for ClickHouse to access Kinesis streams",
    });

    // Create and attach the Kinesis access policy
    const kinesisAccessPolicy = new ManagedPolicy(this, "KinesisAccessPolicy", {
      managedPolicyName: `KinesisAccessPolicy-${id}`,
      document: new PolicyDocument({
        statements: [
          new PolicyStatement({
            effect: Effect.ALLOW,
            actions: [
              "kinesis:DescribeStream",
              "kinesis:GetShardIterator",
              "kinesis:GetRecords",
              "kinesis:ListShards",
              "kinesis:SubscribeToShard",
              "kinesis:DescribeStreamConsumer",
              "kinesis:RegisterStreamConsumer",
              "kinesis:DeregisterStreamConsumer",
              "kinesis:ListStreamConsumers",
            ],
            resources: [otlpKinesisStream.streamArn],
          }),
          new PolicyStatement({
            effect: Effect.ALLOW,
            actions: ["kinesis:ListStreams"],
            resources: ["*"],
          }),
        ],
      }),
    });

    // Attach the policy to the role
    clickHouseAccessRole.addManagedPolicy(kinesisAccessPolicy);

    // Output the role ARN
    new CfnOutput(this, "ClickHouseAccessRoleArn", {
      description: "ARN of the ClickHouse access role",
      value: clickHouseAccessRole.roleArn,
    });

    //==============================================================================
    // RUST EXTENSION LAYER
    //==============================================================================

    // Create Rust Extension for Kinesis Export
    const kinesisExportExtension = new RustExtension(
      this,
      "KinesisExportExtension",
      {
        layerVersionName: "kinesis-export-extension",
        manifestPath: join(
          __dirname,
          "..",
          "layers/kinesis-export",
          "Cargo.toml",
        ),
        bundling: { cargoLambdaFlags: ["--quiet", "--arm64"] },
      },
    );

    //==============================================================================
    // CLIENT RUST LAMBDA
    //==============================================================================

    // Client Rust Lambda
    const clientRustLambda = new RustFunction(this, "ClientRustLambda", {
      functionName: "client-rust-lambda",
      manifestPath: join(__dirname, "..", "functions/client", "Cargo.toml"),
      bundling: { cargoLambdaFlags: ["--quiet"] },
      architecture: Architecture.ARM_64,
      memorySize: 1024,
      timeout: Duration.minutes(1),
      loggingFormat: LoggingFormat.JSON,
      layers: [kinesisExportExtension],
      environment: {
        RUST_LOG: "info",
        OTEL_SERVICE_NAME: "client-rust-lambda",
        OTLP_STDOUT_KINESIS_STREAM_NAME: otlpKinesisStream.streamName,
      },
    });
    const clientRustLambdaUrl = clientRustLambda.addFunctionUrl({
      authType: FunctionUrlAuthType.NONE,
    });

    // Grant permissions to write to Kinesis stream
    otlpKinesisStream.grantWrite(clientRustLambda);

    //==============================================================================
    // OUTPUTS
    //==============================================================================

    // Output the Kinesis stream ARN and name for reference
    new CfnOutput(this, "KinesisStreamArn", {
      description: "ARN of the Kinesis stream",
      value: otlpKinesisStream.streamArn,
    });

    new CfnOutput(this, "KinesisStreamName", {
      description: "Name of the Kinesis stream",
      value: otlpKinesisStream.streamName,
    });

    new CfnOutput(this, "ClientRustLambdaUrl", {
      value: clientRustLambdaUrl.url,
    });
  }
}
