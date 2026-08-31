# On-chain Nexus tools (Sui Move)

On-chain tools are Move modules that execute inside a Sui transaction. There
is no separate hosting — the Sui validator network runs them. Higher
per-call cost (Sui gas), zero infra to maintain.

## When to choose on-chain

- The tool's effect must be verifiable / auditable on-chain.
- The tool modifies on-chain state (transfers, registry writes, NFT mints).
- Trust-minimization matters more than per-call cost.
- Pure deterministic logic with no Web2 dependency.

If the tool calls an external HTTP API, needs secrets, or does heavy
compute, build it off-chain (Rust). On-chain tools are for the
verifiability layer.

## Module shape (from `onchain-tool-development.md`)

```move
module my_onchain_tool::my_onchain_tool;

use nexus_primitives::data;
use nexus_primitives::proof_of_uid::ProofOfUID;
use nexus_primitives::tagged_output::{Self, TaggedOutput};
use sui::bag::{Self, Bag};
use sui::clock::Clock;
use sui::transfer::share_object;
use std::ascii::String as AsciiString;

public struct MY_ONCHAIN_TOOL has drop {}

public struct MyToolWitness has key, store { id: UID }

public struct MyToolState has key {
    id: UID,
    witness: Bag,
    // application-specific fields here
}

public enum Output {
    Ok { result: u64 },
    Err { reason: AsciiString },
    // additional variants as needed; names starting with `err` are
    // treated as error variants by Nexus.
}

fun init(_otw: MY_ONCHAIN_TOOL, ctx: &mut TxContext) {
    let state = MyToolState {
        id: object::new(ctx),
        witness: {
            let mut bag = bag::new(ctx);
            bag.add(b"witness", MyToolWitness { id: object::new(ctx) });
            bag
        },
    };
    share_object(state);
}

/// CRITICAL REQUIREMENTS:
/// 1. First parameter: `worksheet: &mut ProofOfUID`
/// 2. Last parameter:  `ctx: &mut TxContext`
/// 3. Return type:      `TaggedOutput`
/// 4. Stamp the worksheet with the witness ID before returning.
public fun execute(
    worksheet: &mut ProofOfUID,
    state: &mut MyToolState,
    input_value: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): TaggedOutput {
    let witness = state.witness();
    worksheet.stamp_with_data(&witness.id, b"my_tool_executed");

    if (input_value == 0) {
        tagged_output::new(b"err")
            .with_named_payload(b"reason", data::inline_one(b"Input value cannot be zero").as_string())
    } else {
        let result = input_value * 2;
        tagged_output::new(b"ok")
            .with_named_payload(b"result", data::inline_one(result.to_string().into_bytes()).as_number())
    }
}

fun witness(self: &MyToolState): &MyToolWitness {
    self.witness.borrow(b"witness")
}

public fun witness_id(self: &MyToolState): ID {
    self.witness().id.to_inner()
}

#[test_only]
public fun init_for_test(otw: MY_ONCHAIN_TOOL, ctx: &mut TxContext) {
    init(otw, ctx);
}
```

## TaggedOutput value typing

```move
.with_named_payload(b"count",    data::inline_one(value.to_string().into_bytes()).as_number())
.with_named_payload(b"message",  data::inline_one(b"hello").as_string())
.with_named_payload(b"success",  data::inline_one(b"true").as_bool())
.with_named_payload(b"sender",   data::inline_one(address.to_string().into_bytes()).as_address())
.with_named_payload(b"metadata", data::inline_one(b"{\"k\":\"v\"}").as_raw())
.with_named_payload(b"items",    data::inline_many(items).as_number())
```

## Deployment

```sh
# 1. Publish the Move package.
sui client publish --gas-budget 200000000 --json
# Capture packageId and the witness object id from the output.

# 2. Register with Nexus.
nexus tool register onchain \
  --module-path "$PACKAGE_ID::my_onchain_tool" \
  --tool-fqn "xyz.taluslabs.<category>.<service>@1" \
  --description "<one-liner>" \
  --witness-id "$WITNESS_ID"

# 3. Verify.
nexus tool list
```

The witness ID lives in a dynamic field of the shared state object:
`0x2::dynamic_field::Field<vector<u8>, $PACKAGE_ID::my_onchain_tool::MyToolWitness>`.

Find it via the Sui explorer or `sui client object <DYNAMIC_FIELD_ID>`.
