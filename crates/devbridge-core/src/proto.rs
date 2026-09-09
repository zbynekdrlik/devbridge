// Tonic-generated gRPC service trait methods return `Result<_, tonic::Status>`,
// which clippy (rustc 1.98) flags as `result_large_err` because `tonic::Status`
// is larger than the lint's default threshold. This is generated code we don't
// control the shape of — allow the lint for this module only, not workspace-wide.
#![allow(clippy::result_large_err)]

tonic::include_proto!("devbridge.v1");
