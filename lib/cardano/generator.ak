////  This Source Code Form is subject to the terms of the Mozilla Public
////  License, v. 2.0. If a copy of the MPL was not distributed with this
////  file, You can obtain one at http://mozilla.org/MPL/2.0/.

use aiken/collection/list
use aiken/crypto.{ScriptHash}
use aiken/fuzz.{
  and_then, bytearray_between, constant, int, int_at_least, int_between,
  list_between, map, option,
}
use cardano/address.{Address, Inline, Script, VerificationKey}
use cardano/assets.{AssetName, PolicyId, Value, ada_asset_name, ada_policy_id}
use cardano/transaction.{
  Datum, DatumHash, InlineDatum, Input, NoDatum, Output, OutputReference,
}

pub fn any_asset_name() -> Fuzzer<AssetName> {
  bytearray_between(0, 32)
}

pub fn any_policy_id() -> Fuzzer<PolicyId> {
  bytearray_between(28, 28)
}

pub fn any_ada_only_value() -> Fuzzer<Value> {
  map(int_at_least(1), assets.from_lovelace)
}

pub fn any_assets(policy_id: PolicyId, value: Value) -> Fuzzer<Value> {
  let names <- and_then(list_between(any_asset_name(), 0, 2))
  list.foldr(
    names,
    constant(value),
    fn(asset_name, step) {
      let value <- and_then(step)
      let quantity <- map(int_at_least(1))
      value |> assets.add(policy_id, asset_name, quantity)
    },
  )
}

pub fn any_value_with(forced_policy_id: PolicyId) -> Fuzzer<Value> {
  let value <- and_then(any_ada_only_value())
  let policies <- and_then(list_between(any_policy_id(), 0, 2))
  list.foldr(
    [forced_policy_id, ..policies],
    constant(value),
    fn(policy_id, step) {
      let value <- and_then(step)
      any_assets(policy_id, value)
    },
  )
}

pub fn any_value() -> Fuzzer<Value> {
  let value <- and_then(any_ada_only_value())
  let policies <- and_then(list_between(any_policy_id(), 0, 2))
  list.foldr(
    policies,
    constant(value),
    fn(policy_id, step) {
      let value <- and_then(step)
      any_assets(policy_id, value)
    },
  )
}

pub fn any_value_extending(value: Value) -> Fuzzer<Value> {
  let extra_lovelace <-
    and_then(
      if assets.lovelace_of(value) == 0 {
        int_at_least(1)
      } else {
        int()
      },
    )
  let policies <- and_then(list_between(any_policy_id(), 0, 2))
  list.foldr(
    policies,
    constant(assets.add(value, ada_policy_id, ada_asset_name, extra_lovelace)),
    fn(policy_id, step) {
      let value <- and_then(step)
      any_assets(policy_id, value)
    },
  )
}

pub fn any_address() -> Fuzzer<Address> {
  let address_type <- and_then(int_between(0, 5))
  when address_type is {
    0 -> {
      let payment <- map(bytearray_between(28, 28))
      Address {
        payment_credential: VerificationKey(payment),
        stake_credential: None,
      }
    }
    1 -> {
      let payment <- map(bytearray_between(28, 28))
      Address { payment_credential: Script(payment), stake_credential: None }
    }
    2 -> {
      let payment <- and_then(bytearray_between(28, 28))
      let stake <- map(bytearray_between(28, 28))
      Address {
        payment_credential: VerificationKey(payment),
        stake_credential: Some(Inline(VerificationKey(stake))),
      }
    }
    3 -> {
      let payment <- and_then(bytearray_between(28, 28))
      let stake <- map(bytearray_between(28, 28))
      Address {
        payment_credential: VerificationKey(payment),
        stake_credential: Some(Inline(Script(stake))),
      }
    }
    4 -> {
      let payment <- and_then(bytearray_between(28, 28))
      let stake <- map(bytearray_between(28, 28))
      Address {
        payment_credential: Script(payment),
        stake_credential: Some(Inline(VerificationKey(stake))),
      }
    }
    5 -> {
      let payment <- and_then(bytearray_between(28, 28))
      let stake <- map(bytearray_between(28, 28))
      Address {
        payment_credential: Script(payment),
        stake_credential: Some(Inline(Script(stake))),
      }
    }
    _ -> fail @"unexpected address type"
  }
}

pub fn any_datum() -> Fuzzer<Datum> {
  let variant <- and_then(int_between(0, 2))
  when variant is {
    0 -> constant(NoDatum)
    1 -> {
      let hash <- map(bytearray_between(32, 32))
      DatumHash(hash)
    }
    2 -> {
      let i <- map(int())
      InlineDatum(i)
    }
    _ -> fail @"unexpected datum variant"
  }
}

pub fn any_reference_script() -> Fuzzer<Option<ScriptHash>> {
  option(bytearray_between(28, 28))
}

pub fn any_output() -> Fuzzer<Output> {
  let address <- and_then(any_address())
  let value <- and_then(any_value())
  let datum <- and_then(any_datum())
  let reference_script <- map(any_reference_script())
  Output { address, value, datum, reference_script }
}

pub fn any_zero_output() -> Fuzzer<Output> {
  let address <- and_then(any_address())
  let datum <- and_then(any_datum())
  let reference_script <- map(any_reference_script())
  Output { address, value: assets.zero, datum, reference_script }
}

pub fn any_output_reference() -> Fuzzer<OutputReference> {
  let transaction_id <- and_then(bytearray_between(32, 32))
  let output_index <- map(int_between(0, 300))
  OutputReference { transaction_id, output_index }
}

pub fn any_input() -> Fuzzer<Input> {
  let output_reference <- and_then(any_output_reference())
  let output <- map(any_output())
  Input { output_reference, output }
}
