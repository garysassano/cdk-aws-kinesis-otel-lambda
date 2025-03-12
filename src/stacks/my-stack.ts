import {
  CfnOutput,
  Duration,
  RemovalPolicy,
  Stack,
  StackProps,
} from "aws-cdk-lib";
import {
  EndpointType,
  LambdaIntegration,
  RestApi,
} from "aws-cdk-lib/aws-apigateway";
import { AttributeType, TableV2 } from "aws-cdk-lib/aws-dynamodb";
import { Stream, StreamMode } from "aws-cdk-lib/aws-kinesis";
import {
  Architecture,
  FunctionUrlAuthType,
  LoggingFormat,
} from "aws-cdk-lib/aws-lambda";
import { RustFunction } from "cargo-lambda-cdk";
import { Construct } from "constructs";
import { join } from "path";

// Constants
const OTEL_EXPORTER_OTLP_PROTOCOL = "http/protobuf";
const OTEL_EXPORTER_OTLP_COMPRESSION = "gzip";
const OTEL_EXPORTER_OTLP_ENDPOINT = "https://api.honeycomb.io:443";

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
    // DYNAMODB
    //==============================================================================

    const quotesTable = new TableV2(this, "QuotesTable", {
      tableName: "quotes-table",
      partitionKey: { name: "pk", type: AttributeType.STRING },
      timeToLiveAttribute: "expiry",
      removalPolicy: RemovalPolicy.DESTROY,
    });

    //==============================================================================
    // API GATEWAY
    //==============================================================================

    const backendApi = new RestApi(this, "BackendApi", {
      restApiName: "backend-api",
      endpointTypes: [EndpointType.REGIONAL],
    });
    backendApi.node.tryRemoveChild("Endpoint");

    //==============================================================================
    // LAMBDA - SERVICE FUNCTIONS
    //==============================================================================

    // Backend Lambda
    const backendLambda = new RustFunction(this, "BackendLambda", {
      functionName: "backend-lambda",
      manifestPath: join(__dirname, "..", "functions/service", "Cargo.toml"),
      binaryName: "backend",
      bundling: { cargoLambdaFlags: ["--quiet"] },
      architecture: Architecture.ARM_64,
      memorySize: 128,
      timeout: Duration.minutes(1),
      loggingFormat: LoggingFormat.JSON,
      environment: {
        RUST_LOG: "info",
        TABLE_NAME: quotesTable.tableName,
        OTEL_SERVICE_NAME: "backend-lambda",
        OTEL_EXPORTER_OTLP_ENDPOINT,
        OTEL_EXPORTER_OTLP_PROTOCOL,
        OTEL_EXPORTER_OTLP_COMPRESSION,
        OTLP_STDOUT_KINESIS_STREAM_NAME: otlpKinesisStream.streamName,
      },
    });
    quotesTable.grantReadWriteData(backendLambda);

    // Frontend Lambda
    const frontendLambda = new RustFunction(this, "frontendLambda", {
      functionName: "frontend-lambda",
      manifestPath: join(__dirname, "..", "functions/service", "Cargo.toml"),
      binaryName: "frontend",
      bundling: { cargoLambdaFlags: ["--quiet"] },
      architecture: Architecture.ARM_64,
      memorySize: 128,
      timeout: Duration.minutes(1),
      loggingFormat: LoggingFormat.JSON,
      environment: {
        RUST_LOG: "info",
        TARGET_URL: backendApi.url,
        OTEL_SERVICE_NAME: "frontend-lambda",
        OTEL_EXPORTER_OTLP_ENDPOINT,
        OTEL_EXPORTER_OTLP_PROTOCOL,
        OTEL_EXPORTER_OTLP_COMPRESSION,
        OTLP_STDOUT_KINESIS_STREAM_NAME: otlpKinesisStream.streamName,
      },
    });
    frontendLambda.addFunctionUrl({
      authType: FunctionUrlAuthType.NONE,
    });

    //==============================================================================
    // API GATEWAY INTEGRATIONS
    //==============================================================================

    // {api}/quotes
    const quotesResource = backendApi.root.resourceForPath("/quotes");
    quotesResource.addMethod("GET", new LambdaIntegration(backendLambda));
    quotesResource.addMethod("POST", new LambdaIntegration(backendLambda));

    // {api}/quotes/{id}
    const quoteByIdResource = backendApi.root.resourceForPath("/quotes/{id}");
    quoteByIdResource.addMethod("GET", new LambdaIntegration(backendLambda));

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
