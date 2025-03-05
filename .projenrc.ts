import { awscdk, javascript } from "projen";
const project = new awscdk.AwsCdkTypeScriptApp({
  cdkVersion: "2.1.0",
  defaultReleaseBranch: "main",
  name: "cdk-aws-kinesis-otel-lambda",
  packageManager: javascript.NodePackageManager.PNPM,
  prettier: true,
  projenrcTs: true,

  deps: ["cargo-lambda-cdk"],
});
project.synth();
