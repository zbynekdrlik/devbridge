// @ts-ignore - no type defs for 'ipp' npm package
import ipp from 'ipp';

/**
 * Submit an IPP Print-Job with explicit document-name and job-name
 * attributes to the DevBridge test server.
 *
 * The server is assumed to be listening on http://127.0.0.1:16310/ipp/print
 * (matches playwright/test-config.toml).
 */
export async function submitIppJob(opts: {
  ippUrl?: string;
  documentName?: string;
  jobName?: string;
  requestingUser?: string;
}): Promise<void> {
  const url = opts.ippUrl || 'http://127.0.0.1:16310/ipp/print';
  const printer = ipp.Printer(url);

  const operationAttrs: Record<string, string> = {
    'requesting-user-name': opts.requestingUser || 'playwright',
    'document-format': 'application/pdf',
  };
  if (opts.documentName !== undefined) {
    operationAttrs['document-name'] = opts.documentName;
  }
  if (opts.jobName !== undefined) {
    operationAttrs['job-name'] = opts.jobName;
  }

  // Minimal valid PDF — the server writes it to the spool dir and the rest
  // of the pipeline never reads it in this test.
  const minimalPdf = Buffer.from(
    '%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n2 0 obj<</Type/Pages/Count 0>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n',
    'binary'
  );

  const msg = {
    'operation-attributes-tag': operationAttrs,
    data: minimalPdf,
  };

  await new Promise<void>((resolve, reject) => {
    printer.execute('Print-Job', msg, (err: Error | null, res: any) => {
      if (err) {
        reject(new Error(`IPP Print-Job failed: ${err.message}`));
      } else if (res && res.statusCode && !String(res.statusCode).startsWith('successful')) {
        reject(new Error(`IPP server returned status ${res.statusCode}`));
      } else {
        resolve();
      }
    });
  });
}
