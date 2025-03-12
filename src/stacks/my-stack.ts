import { CfnOutput, Duration, Stack, StackProps } from "aws-cdk-lib";
import { Stream, StreamMode } from "aws-cdk-lib/aws-kinesis";
import {
  Architecture,
  FunctionUrlAuthType,
  LoggingFormat,
} from "aws-cdk-lib/aws-lambda";
import { RustFunction } from "cargo-lambda-cdk";
import { Construct } from "constructs";
import { join } from "path";

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
    });

    //==============================================================================
    // CLIENT RUST LAMBDA
    //==============================================================================

    // Client Rust Lambda
    const clientRustLambda = new RustFunction(this, "ClientRustLambda", {
      functionName: "client-rust-lambda",
      manifestPath: join(
        __dirname,
        "..",
        "functions/client/rust",
        "Cargo.toml",
      ),
      bundling: { cargoLambdaFlags: ["--quiet"] },
      architecture: Architecture.ARM_64,
      memorySize: 1024,
      timeout: Duration.minutes(1),
      loggingFormat: LoggingFormat.JSON,
      environment: {
        RUST_LOG: "info",
        OTEL_SERVICE_NAME: "client-rust-lambda",
        OTLP_STDOUT_KINESIS_STREAM_NAME: otlpKinesisStream.streamName,
      },
    });
    const clientRustLambdaUrl = clientRustLambda.addFunctionUrl({
      authType: FunctionUrlAuthType.NONE,
    });

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
