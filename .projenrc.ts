import { awscdk, javascript } from "projen";

const project = new awscdk.AwsCdkTypeScriptApp({
  cdkVersion: "2.181.1",
  defaultReleaseBranch: "main",
  depsUpgradeOptions: { workflow: false },
  devDeps: ["zod"],
  eslint: true,
  gitignore: ["**/target"],
  minNodeVersion: "22.14.0",
  name: "cdk-aws-kinesis-otel-lambda",
  packageManager: javascript.NodePackageManager.PNPM,
  pnpmVersion: "9",
  prettier: true,
  projenrcTs: true,

  deps: ["cargo-lambda-cdk"],
});
project.synth();
