# `xyz.taluslabs.storage.walrus.upload-json@1`

Standard Nexus Tool that uploads a JSON file to Walrus and returns the blob ID.

## Input

**`json`: [`String`]**

The JSON data to upload.

_opt_ **`publisher_url`: [`Option<String>`]** _default_: [`None`]

The Walrus publisher URL. Must be a bare `https://host[:port]` on the endpoint allowlist — no path, query, fragment or credentials. If not provided, the default Walrus configuration will be used. See [Endpoint allowlist](#endpoint-allowlist).

_opt_ **`aggregator_url`: [`Option<String>`]** _default_: [`None`]

The Walrus aggregator URL. Must be a bare `https://host[:port]` on the endpoint allowlist — no path, query, fragment or credentials. If not provided, the default Walrus configuration will be used. See [Endpoint allowlist](#endpoint-allowlist).

_opt_ **`epochs`: [`u64`]** _default_: [`1`]

Number of epochs to store the data.

_opt_ **`send_to_address`: [`Option<String>`]** _default_: [`None`]

Optional address to which the created Blob object should be sent.

## Output Variants & Ports

**`newly_created`**

A new blob was created and uploaded successfully.

- **`newly_created.blob_id`: [`String`]** - The unique identifier for the uploaded blob
- **`newly_created.end_epoch`: [`u64`]** - The epoch at which the blob will expire
- **`newly_created.sui_object_id`: [`String`]** - Sui object ID of the newly created blob

**`already_certified`**

The blob was already certified in the blockchain.

- **`already_certified.blob_id`: [`String`]** - The unique identifier for the blob
- **`already_certified.end_epoch`: [`u64`]** - The epoch at which the blob will expire
- **`already_certified.tx_digest`: [`String`]** - Transaction digest of the certified blob

**`err`**

The blob upload failed.

- **`err.reason`: [`String`]** - A detailed error message describing what went wrong
- **`err.kind`: [`UploadErrorKind`]** - Type of error that occurred
  - Possible kinds:
    - `network` - Error during HTTP requests or network connectivity issues
    - `validation` - Invalid JSON input or data validation failures
- **`err.status_code`: [`Option<u16>`]** - HTTP status code if available (for network errors)

---

# `xyz.taluslabs.storage.walrus.upload-file@1`

Standard Nexus Tool that uploads a file to Walrus and returns the blob ID.

## Input

**`file_path`: [`String`]**

The path of the file to upload, relative to `WALRUS_UPLOAD_ROOT`. Uploading from a local path is **disabled** unless that variable is set. See [Local file uploads](#local-file-uploads).

_opt_ **`publisher_url`: [`Option<String>`]** _default_: [`None`]

The Walrus publisher URL. Must be a bare `https://host[:port]` on the endpoint allowlist — no path, query, fragment or credentials. If not provided, the default Walrus configuration will be used. See [Endpoint allowlist](#endpoint-allowlist).

_opt_ **`epochs`: [`u64`]** _default_: [`1`]

Number of epochs to store the file.

_opt_ **`send_to`: [`Option<String>`]** _default_: [`None`]

Optional address to which the created Blob object should be sent.

## Output Variants & Ports

**`newly_created`**

A new blob was created and uploaded successfully.

- **`newly_created.blob_id`: [`String`]** - The unique identifier for the uploaded blob
- **`newly_created.end_epoch`: [`u64`]** - The epoch at which the blob will expire
- **`newly_created.sui_object_id`: [`String`]** - Sui object ID of the newly created blob

**`already_certified`**

The blob was already certified in the blockchain.

- **`already_certified.blob_id`: [`String`]** - The unique identifier for the blob
- **`already_certified.end_epoch`: [`u64`]** - The epoch at which the blob will expire
- **`already_certified.tx_digest`: [`String`]** - Transaction digest of the certified blob

**`err`**

The file upload failed.

- **`err.reason`: [`String`]** - A detailed error message describing what went wrong
- **`err.kind`: [`UploadErrorKind`]** - Type of error that occurred
  - Possible kinds:
    - `network` - Error during HTTP requests or network connectivity issues
    - `validation` - Invalid file data or file validation failures

# `xyz.taluslabs.storage.walrus.read-json@1`

Standard Nexus Tool that reads a JSON file from Walrus and returns the JSON data. The tool can also validate the JSON data against a provided schema.

## Input

**`blob_id`: [`String`]**

The blob ID of the JSON file to read.

_opt_ **`aggregator_url`: [`Option<String>`]** _default_: [`None`]

The Walrus aggregator URL. Must be a bare `https://host[:port]` on the endpoint allowlist — no path, query, fragment or credentials. If not provided, the default Walrus configuration will be used. See [Endpoint allowlist](#endpoint-allowlist).

_opt_ **`json_schema`: [`Option<WalrusJsonSchema>`]** _default_: [`None`]

Optional JSON schema to validate the data against.

### WalrusJsonSchema Structure

- **`name`: [`String`]** - The name of the schema. Must match `[a-zA-Z0-9-_]`, with a maximum length of 64.
- **`schema`: [`schemars::Schema`]** - The JSON schema for the expected output.
- **`description`: [`Option<String>`]** - A description of the expected format.
- **`strict`: [`Option<bool>`]** - Whether to enable strict schema adherence when validating the output.

## Output Variants & Ports

**`ok`**

The JSON data was read successfully.

- **`ok.json`: [`Value`]** - The JSON data as a structured value

**`err`**

The JSON read operation failed.

- **`err.reason`: [`String`]** - A detailed error message describing what went wrong
- **`err.kind`: [`ReadErrorKind`]** - Type of error that occurred
  - Possible kinds:
    - `network` - Error during HTTP requests or network connectivity issues
    - `validation` - Invalid JSON data format or parsing failures
    - `schema` - Error validating the JSON against the provided schema
- **`err.status_code`: [`Option<u16>`]** - HTTP status code if available (for network errors)

---

# `xyz.taluslabs.storage.walrus.read-file@1`

Standard Nexus Tool that reads a file from Walrus and returns its content as bytes.

## Input

**`blob_id`: [`String`]**

The unique identifier of the blob to read.

_opt_ **`aggregator_url`: [`Option<String>`]** _default_: [`None`]

The Walrus aggregator URL. Must be a bare `https://host[:port]` on the endpoint allowlist — no path, query, fragment or credentials. If not provided, the default Walrus configuration will be used. See [Endpoint allowlist](#endpoint-allowlist).

## Output Variants & Ports

**`ok`**

The file was read successfully.

- **`ok.bytes`: [`Vec<u8>`]** - The file content as a byte array

**`err`**

The file read operation failed.

- **`err.reason`: [`String`]** - A detailed error message describing what went wrong
- **`err.kind`: [`ReadErrorKind`]** - Type of error that occurred
  - Possible kinds:
    - `network` - Error during HTTP requests or network connectivity issues
- **`err.status_code`: [`Option<u16>`]** - HTTP status code if available (for network errors)

---

# `xyz.taluslabs.storage.walrus.verify-blob@1`

Standard Nexus Tool that verifies a blob in Walrus.

## Input

**`blob_id`: [`String`]**

The ID of the blob to verify.

_opt_ **`aggregator_url`: [`Option<String>`]** _default_: [`None`]

The Walrus aggregator URL. Must be a bare `https://host[:port]` on the endpoint allowlist — no path, query, fragment or credentials. If not provided, the default Walrus configuration will be used. See [Endpoint allowlist](#endpoint-allowlist).

## Output Variants & Ports

**`verified`**

The blob exists and is verified.

- **`verified.blob_id`: [`String`]** - The ID of the verified blob

**`unverified`**

The blob does not exist or could not be verified.

- **`unverified.blob_id`: [`String`]** - The ID of the unverified blob

**`err`**

An error occurred during verification.

- **`err.reason`: [`String`]** - A detailed error message describing what went wrong
- **`err.kind`: [`UploadErrorKind`]** - Type of error that occurred
  - Possible kinds:
    - `server` - Server-side errors during verification
- **`err.status_code`: [`Option<u16>`]** - HTTP status code if available (for API errors)

---

# Configuration

## Endpoint allowlist

`publisher_url` and `aggregator_url` are caller-supplied, so they are confined
to an allowlist of hosts. Anything else is refused while the input is being
deserialized, before any request is made.

The allowlist is a compile-time constant (`ALLOWED_HOSTS` in `src/utils.rs`),
not a deployment variable — which storage network these tools talk to is a
property of the tool, so widening it is a reviewed code change. It currently
holds `walrus.space` and its subdomains (the public Walrus publishers and
aggregators, including the SDK's defaults) and the mainnet publisher the leader
is configured against.

A deployment's own `WALRUS_PUBLISHER_URL` / `WALRUS_AGGREGATOR_URL` hosts are
allowed too, matched exactly, so a caller naming the endpoint the tool would
have used anyway is never refused.

A URL must also be a bare `https://host[:port]`, with no path, query, fragment
or credentials. That is not cosmetic — the SDK builds request URLs by
concatenation (`{base}/v1/blobs/{id}`), so a base ending in `#` swallows
everything appended to it and turns a blob read into a request for the host's
root.

## Local file uploads

`upload-file`'s `file_path` is confined to one directory, named by
**`WALRUS_UPLOAD_ROOT`**. The path is interpreted relative to that directory
and cannot leave it: absolute paths and `..` are refused outright, and
containment is re-checked after symlink resolution.

With `WALRUS_UPLOAD_ROOT` unset — the default, and how the hosted tools run —
`file_path` is refused entirely. A hosted instance of this tool has its
toolkit signing key on a mounted volume and nothing a caller would legitimately
want published, so a reachable local-read port there is only useful for
exfiltrating secrets into public storage. Turn it on deliberately, pointed at a
directory that holds the files you mean to publish, and nothing else.

Callers that want to publish content they supply themselves should use
`upload-json`, which takes the bytes inline.
