# Offchain Tools — Bootstrap Notes

After this branch (`feat/offchain-tools-pipeline`) merges to `main`, the
chain pipeline has never run anywhere. The first deploy requires extra
care because the readiness gate compares against artifacts that do not
yet exist.

## Prerequisites (one-time)

Per environment (`testnet`, `mainnet`):

1. **GCP project** (`GCP_PROJECT_ID`): owns the GCS bucket and Secret
   Manager. Bucket name: `<project-id>-nexus-tools`.
2. **Workload identity pool provider** (`nexus-tools-protocol`) bound
   to this repo's `id-token`. Permissions:
   - `roles/storage.objectAdmin` on the bucket.
   - `roles/secretmanager.admin` on the project.
3. **Infra project** (`GCP_INFRA_PROJECT_ID`): hosts GCR + the
   `nexus-sdk/shell` and `generate-signed-http-keys` images.
4. **Workload identity pool provider** (`nexus-tools`) bound to this
   repo's `id-token`. Permissions:
   - `roles/artifactregistry.writer` on the GCR registry.
   - `roles/artifactregistry.reader` on `gcr.io/<infra>/nexus-sdk/*` and
     `gcr.io/<infra>/nexus-next/generate-signed-http-keys`.
5. **GitHub environments** named `testnet` and `mainnet` with these vars:
   - `GCP_PROJECT_ID`, `GCP_PROJECT_NUMBER`
   - `GCP_INFRA_PROJECT_ID`, `GCP_INFRA_PROJECT_NUMBER`
   - `SUI_NETWORK` (`testnet` or `mainnet`)
   - `NEXT_PUBLIC_SUI_RPC_URL`
   - `NEXUS_TAG`, `SUI_CHANNEL`, `SUI_CACHE_VERSION`
6. **GitHub environment secrets**:
   - `SUI_DEPLOYER_MNEMONIC`
   - `GPG_DEVOPS_SIGNING_KEY`

## First deploy

1. Confirm `main` is at the desired commit.
2. Cut a long-lived `testnet` branch from `main`. `git checkout -b testnet && git push -u origin testnet`.
3. The push triggers `CI`, which runs the full chain pipeline against
   the `testnet` environment. Watch the run.
4. On success, every tool has:
   - An image at both registries.
   - A Cloud Run config in
     `gs://<bucket>/testnet/offchain/tools/<tool>-v<version>.json`.
   - Signed-HTTP keys + toolkit-config secrets in Secret Manager.
   - At least one FQN registered on-chain.
5. Repeat for `mainnet` once that branch is cut.

## Subsequent deploys

Use the promote flow described in the spec — `promote/<topic>` PRs +
`workflow_dispatch` with PR number.
