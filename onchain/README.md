# onchain/

Onchain (Move / Sui package) Nexus tools. Each tool is a self-contained Move
package with `sources/`, `tests/`, and canonical `schemas/`.

## Tools

- [`evm-signin`](evm-signin/README.md) — `xyz.taluslabs.evm.verify-signature@1`.
  Verifies an Ethereum secp256k1 signature onchain and recovers the signer's
  `0x` address. Ships `evm_access` for replay-safe MetaMask → Sui identity
  binding (`EvmIdentityPass` + `EvmBindingRegistry`), a reusable
  `assert_controls` gate, and an example `claim_member_badge` gated action.

Each package builds and tests against `nexus-next` as a sibling checkout of
`nexus-tools` (see each tool's `Move.toml` local paths):

```bash
cd onchain/<tool>
sui move build
sui move test
```
