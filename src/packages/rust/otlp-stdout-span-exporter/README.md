# OTLP Stdout Span Exporter

A span exporter that writes OpenTelemetry spans to stdout in OTLP format as plain JSON.

This crate provides an implementation of OpenTelemetry's `SpanExporter` that writes spans to stdout in OTLP (OpenTelemetry Protocol) format as plain JSON. It is particularly useful in serverless environments like AWS Lambda where writing to stdout is a common pattern for exporting telemetry data.

## Features

- Outputs OTLP data directly as plain JSON
- Optionally outputs in ClickHouse-compatible format
- Simple, lightweight implementation
- No compression or encoding overhead

## Usage

### OTLP/JSON Format

```rust
use opentelemetry::trace::{Tracer, TracerProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use otlp_stdout_span_exporter::OtlpStdoutSpanExporter;

#[tokio::main]
async fn main() {
    // Create a new stdout exporter
    let exporter = OtlpStdoutSpanExporter::new();

    // Create a new tracer provider with batch export
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    // Create a tracer
    let tracer = provider.tracer("my-service");

    // Create spans
    tracer.in_span("parent-operation", |_cx| {
        println!("Doing work...");
        
        // Create nested spans
        tracer.in_span("child-operation", |_cx| {
            println!("Doing more work...");
        });
    });
    
    // Shut down the provider
    let _ = provider.shutdown();
}
```

### ClickHouse Format

```rust
use opentelemetry::trace::{Tracer, TracerProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use otlp_stdout_span_exporter::{OtlpStdoutSpanExporter, OutputFormat};

#[tokio::main]
async fn main() {
    // Create a new stdout exporter with ClickHouse format
    let exporter = OtlpStdoutSpanExporter::with_format(OutputFormat::ClickHouse);

    // Create a new tracer provider with batch export
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    // Create a tracer
    let tracer = provider.tracer("hello-world");

    // Create spans
    tracer.in_span("parent-operation", |_cx| {
        println!("Doing work...");
        
        // Create nested spans
        tracer.in_span("child1", |_cx| {
            println!("Doing work in child1...");
        });
        
        tracer.in_span("child2", |_cx| {
            println!("Doing work in child2...");
        });
    });
    
    // Shut down the provider
    let _ = provider.shutdown();
}
```

## Output Formats

### OTLP/JSON Format

The default format is the standard OTLP/JSON format:

```json
{
  "resourceSpans": [
    {
      "resource": {
        "attributes": [
          {
            "key": "service.name",
            "value": {
              "stringValue": "my-service"
            }
          }
        ]
      },
      "scopeSpans": [
        {
          "scope": {
            "name": "my-library",
            "version": "1.0.0"
          },
          "spans": [
            {
              "traceId": "...",
              "spanId": "...",
              "name": "my-span",
              "kind": "SPAN_KIND_INTERNAL",
              "startTimeUnixNano": "...",
              "endTimeUnixNano": "..."
            }
          ]
        }
      ]
    }
  ]
}
```

### ClickHouse Format

The ClickHouse format is compatible with the ClickHouse exporter schema:

```json
[
  {
    "Timestamp": "2023-01-01 12:34:56.789012",
    "TraceId": "8d83651d898e156168070f8aa2e32b4a",
    "SpanId": "31109336dab317a9",
    "ParentSpanId": "610f0841592a8b7b",
    "TraceState": "",
    "SpanName": "GET",
    "SpanKind": "Client",
    "ServiceName": "my-service",
    "ResourceAttributes": {
      "service.name": "my-service",
      "telemetry.sdk.language": "rust"
    },
    "ScopeName": "my-library",
    "ScopeVersion": "1.0.0",
    "SpanAttributes": {
      "http.method": "GET",
      "http.url": "https://example.com"
    },
    "Duration": 1330000,
    "StatusCode": "Unset",
    "StatusMessage": "",
    "Events": {
      "Timestamp": [],
      "Name": [],
      "Attributes": []
    },
    "Links": {
      "TraceId": [],
      "SpanId": [],
      "TraceState": [],
      "Attributes": []
    }
  }
]
```
