import path from 'node:path';
import * as grpc from '@grpc/grpc-js';
import * as protoLoader from '@grpc/proto-loader';

/** The repo's own contract — the same file the server is built from. */
const PROTO = path.resolve(__dirname, '../../../proto/devbridge.proto');

/** `JobCompletion` (proto field names), as a DevBridge client sends it. */
export interface JobCompletion {
  job_id: string;
  success: boolean;
  /** #114: the printer accepted the job, its end was never confirmed. */
  unconfirmed?: boolean;
  error_detail?: string;
  verification_method?: string;
  verification_evidence?: string;
  client_id?: string;
}

/**
 * Report a job's end over the test server's gRPC port, exactly like a
 * client at the end of its print (`CompleteJob`). The server is the one
 * `playwright/test-config.toml` starts (gRPC 127.0.0.1:50051).
 */
export async function completeJob(
  completion: JobCompletion,
  address = '127.0.0.1:50051'
): Promise<void> {
  const definition = protoLoader.loadSync(PROTO, {
    keepCase: true,
    longs: String,
    enums: String,
    defaults: true,
    oneofs: true,
  });
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const loaded = grpc.loadPackageDefinition(definition) as any;
  const client = new loaded.devbridge.v1.PrintBridge(address, grpc.credentials.createInsecure());
  try {
    await new Promise<void>((resolve, reject) => {
      const deadline = new Date(Date.now() + 10_000);
      client.CompleteJob(completion, { deadline }, (err: grpc.ServiceError | null) => {
        if (err) {
          reject(new Error(`CompleteJob failed: ${err.message}`));
        } else {
          resolve();
        }
      });
    });
  } finally {
    client.close();
  }
}
