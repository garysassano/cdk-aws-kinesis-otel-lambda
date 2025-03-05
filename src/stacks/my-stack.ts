import { Schedule, ScheduleExpression } from "@aws-cdk/aws-scheduler-alpha";
import { LambdaInvoke } from "@aws-cdk/aws-scheduler-targets-alpha";
import {
  CfnOutput,
  DockerImage,
  Duration,
  RemovalPolicy,
  SecretValue,
  Stack,
  StackProps,
} from "aws-cdk-lib";
import {
  EndpointType,
  LambdaIntegration,
  RestApi,
} from "aws-cdk-lib/aws-apigateway";
import { AttributeType, TableV2 } from "aws-cdk-lib/aws-dynamodb";
import {
  ArnPrincipal,
  Effect,
  PolicyStatement,
  Role,
  ServicePrincipal,
} from "aws-cdk-lib/aws-iam";
import { Stream, StreamMode } from "aws-cdk-lib/aws-kinesis";
import {
  Architecture,
  FunctionUrlAuthType,
  LoggingFormat,
  Runtime,
} from "aws-cdk-lib/aws-lambda";
import { NodejsFunction } from "aws-cdk-lib/aws-lambda-nodejs";
import {
  FilterPattern,
  LogGroup,
  RetentionDays,
  SubscriptionFilter,
} from "aws-cdk-lib/aws-logs";
import { LambdaDestination } from "aws-cdk-lib/aws-logs-destinations";
import { Secret } from "aws-cdk-lib/aws-secretsmanager";
import { RustFunction } from "cargo-lambda-cdk";
import { Construct } from "constructs";
import { join } from "path";
import { PythonFunction } from "uv-python-lambda";
import { validateEnv } from "../utils/validate-env";

// Constants
const OTEL_EXPORTER_OTLP_PROTOCOL = "http/protobuf";
const OTEL_EXPORTER_OTLP_COMPRESSION = "gzip";

// Required environment variables
const { OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_HEADERS } = validateEnv(
  ["OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_EXPORTER_OTLP_HEADERS"],
);

export class MyStack extends Stack {
  constructor(scope: Construct, id: string, props: StackProps = {}) {
    super(scope, id, props);

    // Constants for Kinesis configuration
    const COLLECTORS_SECRETS_KEY_PREFIX = "serverless-otlp-forwarder/keys";

    //==============================================================================
    // SECRETS MANAGER
    //==============================================================================

    new Secret(this, "VendorSecret", {
      secretName: `${COLLECTORS_SECRETS_KEY_PREFIX}vendor`,
      description: "Vendor API key for OTLP forwarder",
      secretStringValue: SecretValue.unsafePlainText(
        JSON.stringify({
          name: "vendor",
          endpoint: OTEL_EXPORTER_OTLP_ENDPOINT,
          auth: OTEL_EXPORTER_OTLP_HEADERS,
        }),
      ),
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
    // KINESIS RESOURCES
    //==============================================================================

    // Create Kinesis Stream
    const otlpKinesisStream = new Stream(this, "OtlpKinesisStream", {
      streamName: `${id}-otlp-stream`,
      retentionPeriod: Duration.hours(24),
      streamMode: StreamMode.ON_DEMAND,
    });

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
    const frontendLambdaUrl = frontendLambda.addFunctionUrl({
      authType: FunctionUrlAuthType.NONE,
    });

    // Grant permissions to the frontendLambda to write to the Kinesis stream
    otlpKinesisStream.grantWrite(frontendLambda);

    //==============================================================================
    // CLICKHOUSE CLICKPIPES IAM ROLE
    //==============================================================================

    // Create an IAM role for ClickHouse ClickPipes to access Kinesis
    // NOTE: You must replace 'CLICKHOUSE_IAM_ARN' with the actual ARN provided by ClickHouse
    const CLICKHOUSE_IAM_ARN =
      "arn:aws:iam::426924874929:role/CH-S3-bisque-uh-92-uw2-83-Role"; // Replace this with the actual ARN

    const clickPipesKinesisRole = new Role(this, "ClickPipesKinesisRole", {
      roleName: `ClickHouseAccessRole-${id}`, // Role name must start with ClickHouseAccessRole-
      description: "Role for ClickHouse ClickPipes to access Kinesis stream",
      // Trust policy to allow ClickHouse to assume this role
      assumedBy: new ArnPrincipal(CLICKHOUSE_IAM_ARN),
    });

    // Add permissions to read from the Kinesis stream - using the exact permissions required
    clickPipesKinesisRole.addToPolicy(
      new PolicyStatement({
        effect: Effect.ALLOW,
        actions: [
          "kinesis:DescribeStream",
          "kinesis:GetShardIterator",
          "kinesis:GetRecords",
          "kinesis:ListShards",
          "kinesis:SubscribeToShard",
        ],
        resources: [otlpKinesisStream.streamArn],
      }),
    );

    // Add permissions for consumer operations on the stream
    clickPipesKinesisRole.addToPolicy(
      new PolicyStatement({
        effect: Effect.ALLOW,
        actions: [
          "kinesis:DescribeStreamConsumer",
          "kinesis:RegisterStreamConsumer",
          "kinesis:DeregisterStreamConsumer",
          "kinesis:ListStreamConsumers",
        ],
        resources: [
          otlpKinesisStream.streamArn,
          `${otlpKinesisStream.streamArn}/consumer/*`,
        ],
      }),
    );

    // Add permission to list all streams
    clickPipesKinesisRole.addToPolicy(
      new PolicyStatement({
        effect: Effect.ALLOW,
        actions: ["kinesis:ListStreams"],
        resources: ["*"],
      }),
    );

    // Output the role ARN to be used in ClickHouse ClickPipes configuration
    new CfnOutput(this, "ClickPipesKinesisRoleArn", {
      description:
        "ARN of the IAM role for ClickHouse ClickPipes Kinesis integration",
      value: clickPipesKinesisRole.roleArn,
    });

    // Output the Kinesis stream ARN and name for reference
    new CfnOutput(this, "KinesisStreamArn", {
      description: "ARN of the Kinesis stream",
      value: otlpKinesisStream.streamArn,
    });

    new CfnOutput(this, "KinesisStreamName", {
      description: "Name of the Kinesis stream",
      value: otlpKinesisStream.streamName,
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
    // LAMBDA - CLIENT FUNCTIONS
    //==============================================================================

    // Client Node Lambda
    const clientNodeLambda = new NodejsFunction(this, "ClientNodeLambda", {
      functionName: "client-node-lambda",
      entry: join(__dirname, "..", "functions/client/node", "index.ts"),
      runtime: Runtime.NODEJS_22_X,
      architecture: Architecture.ARM_64,
      memorySize: 1024,
      timeout: Duration.minutes(1),
      loggingFormat: LoggingFormat.JSON,
      environment: {
        TARGET_URL: `${backendApi.url}quotes`,
        OTEL_SERVICE_NAME: "client-node-lambda",
        OTEL_EXPORTER_OTLP_ENDPOINT,
        OTEL_EXPORTER_OTLP_PROTOCOL,
        OTEL_EXPORTER_OTLP_COMPRESSION,
      },
    });
    new Schedule(this, "ClientNodeLambdaSchedule", {
      scheduleName: `client-node-lambda-schedule`,
      description: `Trigger ${clientNodeLambda.functionName} every 5 minutes`,
      schedule: ScheduleExpression.rate(Duration.minutes(5)),
      target: new LambdaInvoke(clientNodeLambda),
    });

    // Client Python Lambda
    const clientPythonLambda = new PythonFunction(this, "ClientPythonLambda", {
      functionName: "client-python-lambda",
      rootDir: join(__dirname, "..", "functions/client/python"),
      runtime: Runtime.PYTHON_3_13,
      architecture: Architecture.ARM_64,
      memorySize: 1024,
      timeout: Duration.minutes(1),
      loggingFormat: LoggingFormat.JSON,
      environment: {
        TARGET_URL: `${backendApi.url}quotes`,
        OTEL_SERVICE_NAME: "client-python-lambda",
        OTEL_EXPORTER_OTLP_ENDPOINT,
        OTEL_EXPORTER_OTLP_PROTOCOL,
        OTEL_EXPORTER_OTLP_COMPRESSION,
      },
      bundling: {
        image: DockerImage.fromBuild(
          join(__dirname, "..", "functions/client/python"),
        ),
        assetExcludes: ["Dockerfile"],
      },
    });
    new Schedule(this, "ClientPythonLambdaSchedule", {
      scheduleName: `client-python-lambda-schedule`,
      description: `Trigger ${clientPythonLambda.functionName} every 5 minutes`,
      schedule: ScheduleExpression.rate(Duration.minutes(5)),
      target: new LambdaInvoke(clientPythonLambda),
    });

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
        OTEL_EXPORTER_OTLP_ENDPOINT,
        OTEL_EXPORTER_OTLP_HEADERS,
        OTEL_EXPORTER_OTLP_PROTOCOL,
        OTEL_EXPORTER_OTLP_COMPRESSION,
      },
    });
    const clientRustLambdaUrl = clientRustLambda.addFunctionUrl({
      authType: FunctionUrlAuthType.NONE,
    });

    //==============================================================================
    // LAMBDA - OTLP FORWARDER
    //==============================================================================

    // Forwarder Lambda
    const forwarderLambda = new RustFunction(this, "ForwarderLambda", {
      functionName: "forwarder-lambda",
      description: `Processes logs from AWS Account ${this.account}`,
      manifestPath: join(__dirname, "..", "functions/forwarder", "Cargo.toml"),
      binaryName: "log_processor",
      bundling: { cargoLambdaFlags: ["--quiet"] },
      architecture: Architecture.ARM_64,
      memorySize: 128,
      loggingFormat: LoggingFormat.JSON,
      environment: {
        RUST_LOG: "info",
        COLLECTORS_SECRETS_KEY_PREFIX,
        COLLECTORS_CACHE_TTL_SECONDS: "300",
        OTEL_SERVICE_NAME: "forwarder-lambda",
        OTEL_EXPORTER_OTLP_ENDPOINT,
        OTEL_EXPORTER_OTLP_HEADERS,
        OTEL_EXPORTER_OTLP_PROTOCOL,
      },
    });
    forwarderLambda.grantInvoke(new ServicePrincipal("logs.amazonaws.com"));
    forwarderLambda.addToRolePolicy(
      new PolicyStatement({
        effect: Effect.ALLOW,
        actions: [
          "secretsmanager:GetSecretValue",
          "secretsmanager:BatchGetSecretValue",
          "secretsmanager:ListSecrets",
        ],
        resources: ["*"],
      }),
    );

    //==============================================================================
    // LAMBDA LAYER - RUST EXTENSION
    //==============================================================================

    // // Create a custom Lambda layer that builds a Rust extension
    // const rustExtensionLayer = new LayerVersion(
    //   this,
    //   "StdoutKinesisOTLPLayer",
    //   {
    //     code: Code.fromAsset(join(__dirname, "..", "functions/extension"), {
    //       bundling: {
    //         image: DockerImage.fromRegistry(
    //           "amazon/aws-sam-cli-build-image-rust",
    //         ),
    //         command: [
    //           "bash",
    //           "-c",
    //           [
    //             "cargo lambda build --release --extension --arm64",
    //             "mkdir -p /asset-output/extensions",
    //             // The following uses jq to get the target directory and then copies the extension to the output
    //             "cp \"$(cargo metadata --format-version=1 | jq -r '.target_directory')/lambda/extensions/otlp-stdout-kinesis-extension-layer\" /asset-output/extensions/",
    //           ].join(" && "),
    //         ],
    //         user: "root",
    //         securityOpt: "seccomp=unconfined",
    //         volumes: [
    //           {
    //             hostPath: "/tmp",
    //             containerPath: "/tmp",
    //           },
    //         ],
    //       },
    //     }),
    //     description: "OTLP Stdout Kinesis Extension Layer",
    //     compatibleArchitectures: [Architecture.ARM_64],
    //     compatibleRuntimes: [
    //       Runtime.PROVIDED_AL2023,
    //       Runtime.NODEJS_22_X,
    //       Runtime.PYTHON_3_13,
    //     ],
    //   },
    // );

    // // Export the layer ARN
    // new CfnOutput(this, "RustExtensionLayerArn", {
    //   value: rustExtensionLayer.layerVersionArn,
    //   description: "ARN of the Rust extension layer",
    // });

    //==============================================================================
    // CLOUDWATCH LOGS
    //==============================================================================

    // Define the lambdas and their configurations
    const lambdaConfigs = [
      { lambda: backendLambda, name: "Backend" },
      { lambda: frontendLambda, name: "Frontend" },
      { lambda: clientNodeLambda, name: "ClientNode" },
      { lambda: clientPythonLambda, name: "ClientPython" },
      { lambda: clientRustLambda, name: "ClientRust" },
    ];

    // Create forwarder subscription for each lambda
    lambdaConfigs.forEach(({ lambda, name }) => {
      const logGroup = new LogGroup(this, `${name}LambdaLogGroup`, {
        logGroupName: `/aws/lambda/${lambda.functionName}`,
        retention: RetentionDays.ONE_WEEK,
        removalPolicy: RemovalPolicy.DESTROY,
      });

      new SubscriptionFilter(this, `${name}LambdaSubscription`, {
        logGroup,
        destination: new LambdaDestination(forwarderLambda),
        filterPattern: FilterPattern.literal("{ $.__otel_otlp_stdout = * }"),
      });

      // Policy to avoid race condition
      // (Lambda could recreate the log group if function is invoked while stack is being destroyed)
      lambda.addToRolePolicy(
        new PolicyStatement({
          effect: Effect.DENY,
          actions: ["logs:CreateLogGroup"],
          resources: ["*"],
        }),
      );
    });

    /*
    // Create security group for VPC deployment if needed
    const hasVpcConfig = USE_VPC;
    let kinesisProcessorSecurityGroup: SecurityGroup | undefined;
    let vpc: IVpc | undefined;

    if (hasVpcConfig) {
      // Use the default VPC for simplicity
      vpc = Vpc.fromLookup(this, "DefaultVpc", { isDefault: true });

      kinesisProcessorSecurityGroup = new SecurityGroup(this, "KinesisProcessorSecurityGroup", {
        vpc,
        description: "Security group for OTLP Kinesis Processor Lambda",
        allowAllOutbound: true,
      });
    }

    // Create Kinesis Processor Lambda
    const kinesisProcessorFunction = new RustFunction(this, "KinesisProcessorFunction", {
      functionName: id,
      description: `Processes OTLP data from Kinesis stream in AWS Account ${Stack.of(this).account}`,
      manifestPath: join(__dirname, "..", "functions/processor", "Cargo.toml"),
      binaryName: "bootstrap",
      architecture: Architecture.ARM_64,
      timeout: Duration.seconds(60),
      environment: {
        RUST_LOG: "info",
        OTEL_SERVICE_NAME: id,
        OTEL_EXPORTER_OTLP_ENDPOINT: `{{resolve:secretsmanager:${COLLECTORS_SECRETS_KEY_PREFIX}/default:SecretString:endpoint}}`,
        OTEL_EXPORTER_OTLP_HEADERS: `{{resolve:secretsmanager:${COLLECTORS_SECRETS_KEY_PREFIX}/default:SecretString:auth}}`,
        OTEL_EXPORTER_OTLP_PROTOCOL: "http/protobuf",
        COLLECTORS_CACHE_TTL_SECONDS: COLLECTORS_CACHE_TTL_SECONDS.toString(),
        COLLECTORS_SECRETS_KEY_PREFIX: COLLECTORS_SECRETS_KEY_PREFIX,
      },
      vpc: hasVpcConfig && vpc ? vpc : undefined,
      vpcSubnets: hasVpcConfig && vpc ? { subnetType: SubnetType.PRIVATE_WITH_EGRESS } : undefined,
      securityGroups: hasVpcConfig && kinesisProcessorSecurityGroup ? [kinesisProcessorSecurityGroup] : undefined,
    });

    // Add event source from Kinesis stream
    kinesisProcessorFunction.addEventSource(new KinesisEventSource(otlpKinesisStream, {
      startingPosition: StartingPosition.LATEST,
      batchSize: 100,
      maxBatchingWindow: Duration.seconds(5),
      reportBatchItemFailures: true,
    }));

    // Add required permissions
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
      })
    );

    kinesisProcessorFunction.addToRolePolicy(
      new PolicyStatement({
        effect: Effect.ALLOW,
        actions: [
          "secretsmanager:GetSecretValue",
        ],
        resources: [`arn:${Stack.of(this).partition}:secretsmanager:${Stack.of(this).region}:${Stack.of(this).account}:secret:${COLLECTORS_SECRETS_KEY_PREFIX}/*`],
      })
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
      })
    );

    // Add outputs
    new CfnOutput(this, "KinesisProcessorFunctionName", {
      description: "Name of the Kinesis processor Lambda function",
      value: kinesisProcessorFunction.functionName,
    });

    new CfnOutput(this, "KinesisProcessorFunctionArn", {
      description: "ARN of the Kinesis processor Lambda function",
      value: kinesisProcessorFunction.functionArn,
    });

    if (hasVpcConfig && kinesisProcessorSecurityGroup) {
      new CfnOutput(this, "KinesisProcessorSecurityGroupId", {
        description: "ID of the security group for the Kinesis processor Lambda function",
        value: kinesisProcessorSecurityGroup.securityGroupId,
      });
    }
    */

    //==============================================================================
    // OUTPUTS
    //==============================================================================

    new CfnOutput(this, "QuotesApiUrl", {
      value: `${backendApi.url}quotes`,
    });

    new CfnOutput(this, "FrontendLambdaUrl", {
      value: frontendLambdaUrl.url,
    });

    new CfnOutput(this, "ClientRustLambdaUrl", {
      value: clientRustLambdaUrl.url,
    });
  }
}
