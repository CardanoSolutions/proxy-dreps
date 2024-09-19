use crate::cardano::ProtocolParameters;
use cardano::Cardano;
use clap::{Arg, ArgAction, ArgGroup, Command};
use indoc::{indoc, printdoc};
use pallas_addresses::{
    Address, Network, ShelleyAddress, ShelleyDelegationPart, ShelleyPaymentPart,
};
use pallas_codec::{
    minicbor as cbor,
    utils::{Bytes, NonEmptyKeyValuePairs, NonEmptySet, NonZeroInt, Nullable, PositiveCoin, Set},
};
use pallas_crypto::hash::{Hash, Hasher};
use pallas_primitives::conway::{
    Anchor, AssetName, Certificate, Constr, DRep, ExUnits, GovActionId, Language, Multiasset,
    NetworkId, PlutusData, PlutusV3Script, PostAlonzoTransactionOutput, PseudoTransactionOutput,
    RedeemerTag, RedeemersKey, RedeemersValue, StakeCredential, TransactionBody, TransactionInput,
    Tx, Value, Vote, Voter, VotingProcedure, WitnessSet,
};
use std::{cmp::Ordering, num, str::FromStr};
use uplc::tx::{eval_phase_two, ResolvedInput, SlotConfig};

mod cardano;

// ------------------------------------------------------------------ main ----

#[tokio::main]
async fn main() -> Result<(), Error> {
    let network = Cardano::new();

    match cli().get_matches().subcommand() {
        Some(("assign-stake", args)) => {
            let validator = hex::decode(args.get_one::<String>("validator").unwrap())
                .map_err(|e| Error::FailedToDecodeHexString("validator", e))?
                .into();

            let fuel = args.get_one::<String>("fuel").unwrap().parse()?;

            report(assign_stake(network, validator, fuel).await?)
        }

        Some(("delegate", args)) => {
            let validator = hex::decode(args.get_one::<String>("validator").unwrap())
                .map_err(|e| Error::FailedToDecodeHexString("validator", e))?
                .into();

            let contract = args
                .get_one::<String>("contract")
                .map(|s| s.parse())
                .transpose()?;

            let administrators = args
                .get_many::<String>("administrator")
                .unwrap_or_default()
                .map(|admin| admin.parse())
                .collect::<Result<Vec<Hash<28>>, _>>()
                .map_err(|e| Error::FailedToDecodeHexString("administrator", e))?;

            let delegates = args
                .get_many::<String>("delegate")
                .unwrap_or_default()
                .map(|delegate| delegate.parse())
                .collect::<Result<Vec<Hash<28>>, _>>()
                .map_err(|e| Error::FailedToDecodeHexString("delegate", e))?;

            let quorum = args
                .get_one::<String>("quorum")
                .map(|s| s.parse().map_err(|e| Error::FailedToDecodeInt("quorum", e)))
                .transpose()?
                .unwrap_or(delegates.len());

            let fuel = args.get_one::<String>("fuel").unwrap().parse()?;

            report(if let Some(contract) = contract {
                redelegate(
                    network,
                    validator,
                    administrators,
                    delegates,
                    quorum,
                    contract,
                    fuel,
                )
                .await?
            } else {
                delegate(network, validator, administrators, delegates, quorum, fuel).await?
            })
        }

        Some(("vote", args)) => {
            let validator = hex::decode(args.get_one::<String>("validator").unwrap())
                .map_err(|e| Error::FailedToDecodeHexString("validator", e))?
                .into();

            let delegates = args
                .get_many::<String>("delegate")
                .unwrap_or_default()
                .map(|delegate| delegate.parse())
                .collect::<Result<Vec<Hash<28>>, _>>()
                .map_err(|e| Error::FailedToDecodeHexString("delegate", e))?;

            let choice = match args.get_one::<clap::Id>("vote").unwrap().as_str() {
                "yes" => Vote::Yes,
                "no" => Vote::No,
                "abstain" => Vote::Abstain,
                _ => unreachable!(),
            };

            let anchor = args.get_one::<String>("anchor").map(|s| s.as_str());

            let OutputReference(utxo_like) = args.get_one::<String>("proposal").unwrap().parse()?;
            let proposal_id = GovActionId {
                transaction_id: utxo_like.transaction_id,
                action_index: utxo_like.index as u32,
            };

            let contract = args.get_one::<String>("contract").unwrap().parse()?;

            let fuel = args.get_one::<String>("fuel").unwrap().parse()?;

            report(
                vote(
                    network,
                    validator,
                    delegates,
                    choice,
                    anchor,
                    proposal_id,
                    contract,
                    fuel,
                )
                .await?,
            )
        }

        Some(("revoke", _)) => Ok(()),

        _ => unreachable!(),
    }
}

struct OutputReference(TransactionInput);

impl FromStr for OutputReference {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match &s.split('#').collect::<Vec<_>>()[..] {
            [tx_id_str, ix_str] => {
                let transaction_id: Hash<32> = tx_id_str
                    .parse()
                    .map_err(|e| Error::FailedToDecodeHexString("transaction id", e))?;
                let index: u64 = ix_str
                    .parse()
                    .map_err(|e| Error::FailedToDecodeInt("output index", e))?;
                Ok(OutputReference(TransactionInput {
                    transaction_id,
                    index,
                }))
            }
            _ => Err(Error::MalformedOutputReference),
        }
    }
}

// ---------------------------------------------------------------- errors ----

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum Error {
    FailedToDecodeHexString(&'static str, hex::FromHexError),
    MalformedOutputReference,
    FailedToDecodeInt(&'static str, num::ParseIntError),
}

// ------------------------------------------------------------------- cli ----

fn cli() -> Command {
    Command::new("Hot/Cold DRep Management")
        .version("1.0.0")
        .about("A toolkit providing hot/cold account management for delegate representatives on Cardano.
This command-line serves as a transaction builder various steps of the contract.")
        .subcommand(
            Command::new("vote")
                .about("Vote on a governance action.")
                .arg(arg_contract(true))
                .arg(arg_validator())
                .arg(arg_delegate())
                .arg(arg_fuel())
                .arg(arg_proposal())
                .arg(arg_anchor())
                .arg(flag_yes())
                .arg(flag_no())
                .arg(flag_abstain())
                .group(arg_vote())
        )
        .subcommand(
            Command::new("delegate")
                .about(indoc! {
                    r#"Hand-over voting rights to a delegate script. The --contract option is only mandatory for re-delegation (as it typically doesn't exist otherwise).
                       Also, the specified --administrator must reflect the signatories for the transaction, but not necessarily ALL administrators. Only those authorizing
                       the transaction must be present. And, there must be enough signatories for a quorum."#
                })
                .arg(arg_validator())
                .arg(arg_delegate())
                .arg(arg_administrator())
                .arg(arg_quorum())
                .arg(arg_contract(false))
                .arg(arg_fuel())
        )
        .subcommand(
            Command::new("assign-stake")
                .about(indoc! {
                    r#"Assign all stake from the fuel input to the DRep contract. This command exists mainly for testing purposes, as an easy way to get some stake into
                       the DRep."#
                })
                .arg(arg_validator())
                .arg(arg_fuel())
        )
        .subcommand(
            Command::new("revoke")
                .about("Revoke delegation, without defining a new delegate.")
        )
}

// ------------------------------------------------------------- arguments ----

fn arg_validator() -> Arg {
    Arg::new("validator")
        .long("validator")
        .short('v')
        .value_name("HEX_STRING")
        .help("The compiled validator code, hex-encoded. (e.g jq -r '.validators[0].compiledCode' plutus.json)")
        .action(ArgAction::Set)
}

fn arg_anchor() -> Arg {
    Arg::new("anchor")
        .long("anchor")
        .short('a')
        .value_name("URL")
        .help("An (optional) URL to an anchor file containing rationale for the vote.")
        .action(ArgAction::Set)
}

fn arg_vote() -> ArgGroup {
    ArgGroup::new("vote")
        .args(["yes", "no", "abstain"])
        .multiple(true)
        .required(true)
}

fn flag_yes() -> Arg {
    Arg::new("yes")
        .short('y')
        .long("yes")
        .help("Approve the governance proposal")
        .action(ArgAction::SetTrue)
}

fn flag_no() -> Arg {
    Arg::new("no")
        .short('n')
        .long("no")
        .help("Reject the governance proposal")
        .action(ArgAction::SetTrue)
}

fn flag_abstain() -> Arg {
    Arg::new("abstain")
        .long("abstain")
        .help("Abstain from the governance proposal voting")
        .action(ArgAction::SetTrue)
}

fn arg_contract(required: bool) -> Arg {
    Arg::new("contract")
        .long("contract")
        .short('c')
        .value_name("TX_ID#IX")
        .help("The UTxO holding the contract's state.")
        .required(required)
        .action(ArgAction::Set)
}

fn arg_fuel() -> Arg {
    Arg::new("fuel")
        .long("fuel")
        .short('f')
        .required(true)
        .value_name("TX_ID#IX")
        .help("A UTxO to use as fuel for the transaction. Must be suitable for collateral use.")
        .action(ArgAction::Set)
}

fn arg_proposal() -> Arg {
    Arg::new("proposal")
        .long("proposal")
        .short('p')
        .required(true)
        .value_name("TX_ID#IX")
        .help("The proposal procedure identifier that's being voted on.")
        .action(ArgAction::Set)
}

fn arg_delegate() -> Arg {
    Arg::new("delegate")
        .long("delegate")
        .short('s')
        .value_name("HEX_STRING")
        .help("Verification key hash digest (blake2b-228) of a delegate signatory. Use multiple times for multiple delegates.")
        .action(ArgAction::Append)
}

fn arg_administrator() -> Arg {
    Arg::new("administrator")
        .long("administrator")
        .short('a')
        .value_name("HEX_STRING")
        .help("Verification key hash digest (blake2b-228) of an admin signatory. Use multiple times for multiple admins.")
        .action(ArgAction::Append)
}

fn arg_quorum() -> Arg {
    Arg::new("quorum")
        .long("quorum")
        .short('q')
        .value_name("UINT")
        .help("Minimum number of delegates to authorize votes. Default to the total number of delegates (plenum).")
        .action(ArgAction::Set)
}

// -------------------------------------------------------------- commands ----

fn report<E>(tx: Tx) -> Result<(), E> {
    let mut buf = Vec::new();
    cbor::encode(tx, &mut buf).unwrap();
    printdoc! {
        r#"{{
          "type": "Unwitnessed Tx ConwayEra",
          "description": "Ledger Cddl Format",
          "cborHex": "{}"
        }}"#,
        hex::encode(&buf),
    };
    Ok(())
}

async fn assign_stake(
    network: Cardano,
    validator: Bytes,
    OutputReference(fuel): OutputReference,
) -> Result<Tx, Error> {
    let (validator_hash, _) = from_validator(validator.as_ref(), network.network_id());

    let params = network.protocol_parameters().await;

    let fuel_output = network
        .resolve(&fuel)
        .await
        .expect("failed to resolve fuel UTxO");

    build_transaction(&params, &[], |fee, _| {
        let inputs = vec![fuel.clone()];

        let (vkh, address) =
            if let Ok(Address::Shelley(src)) = Address::from_bytes(&fuel_output.address) {
                let payment_part = src.payment().clone();
                let (vkh, delegation_part) = match payment_part {
                    ShelleyPaymentPart::Key(vkh) => (vkh, ShelleyDelegationPart::Key(vkh)),
                    ShelleyPaymentPart::Script(..) => unreachable!(),
                };
                (
                    vkh,
                    ShelleyAddress::new(src.network(), payment_part, delegation_part),
                )
            } else {
                unreachable!();
            };

        let total_cost = fee + 2_000_000;

        let outputs = vec![PostAlonzoTransactionOutput {
            address: address.to_vec().into(),
            value: subtract(fuel_output.value.clone(), total_cost).expect("not enough fuel"),
            datum_option: None,
            script_ref: None,
        }];

        let certificates = vec![Certificate::VoteRegDeleg(
            StakeCredential::AddrKeyhash(vkh),
            DRep::Script(validator_hash),
            2_000_000,
        )];

        Tx {
            transaction_body: TransactionBody {
                inputs: Set::from(inputs),
                outputs: outputs
                    .into_iter()
                    .map(PseudoTransactionOutput::PostAlonzo)
                    .collect(),
                fee,
                certificates: Some(NonEmptySet::try_from(certificates).unwrap()),
                ..default_transaction_body()
            },
            transaction_witness_set: default_witness_set(),
            success: true,
            auxiliary_data: Nullable::Null,
        }
    })
}

async fn delegate(
    network: Cardano,
    validator: Bytes,
    administrators: Vec<Hash<28>>,
    delegates: Vec<Hash<28>>,
    quorum: usize,
    OutputReference(fuel): OutputReference,
) -> Result<Tx, Error> {
    let (validator_hash, validator_address) =
        from_validator(validator.as_ref(), network.network_id());

    let params = network.protocol_parameters().await;

    let fuel_output = network
        .resolve(&fuel)
        .await
        .expect("failed to resolve fuel UTxO");

    let resolved_inputs = &[ResolvedInput {
        input: fuel.clone(),
        output: PseudoTransactionOutput::PostAlonzo(fuel_output.clone()),
    }];

    build_transaction(&params, resolved_inputs, |fee, ex_units| {
        let (rules, asset_name) = build_rules(&delegates[..], quorum);

        let contract_output =
            new_min_value_output(params.min_utxo_deposit_coefficient, |lovelace| {
                PostAlonzoTransactionOutput {
                    address: validator_address.to_vec().into(),
                    value: Value::Multiasset(
                        lovelace,
                        singleton_assets(
                            validator_hash,
                            &[(asset_name.clone(), PositiveCoin::try_from(1).unwrap())],
                        ),
                    ),
                    datum_option: None,
                    script_ref: None,
                }
            });

        let total_collateral = (fee as f64 * params.collateral_percent).ceil() as u64;

        let mut redeemers = vec![];

        let inputs = vec![fuel.clone()];

        let total_cost = params.drep_deposit + lovelace_of(&contract_output.value) + fee;

        let outputs = vec![
            // Contract
            contract_output,
            // Change
            PostAlonzoTransactionOutput {
                address: fuel_output.address.clone(),
                value: subtract(fuel_output.value.clone(), total_cost).expect("not enough fuel"),
                datum_option: None,
                script_ref: None,
            },
        ];

        let collateral_return = PostAlonzoTransactionOutput {
            address: fuel_output.address.clone(),
            value: subtract(fuel_output.value.clone(), total_collateral).expect("not enough fuel"),
            datum_option: None,
            script_ref: None,
        };

        let mint = singleton_assets(
            validator_hash,
            &[(asset_name, NonZeroInt::try_from(1).unwrap())],
        );
        redeemers.push((
            RedeemersKey {
                tag: RedeemerTag::Mint,
                index: 0,
            },
            RedeemersValue {
                data: void(),
                ex_units: ex_units[0],
            },
        ));

        let certificates = vec![Certificate::RegDRepCert(
            StakeCredential::Scripthash(validator_hash),
            params.drep_deposit,
            Nullable::Null,
        )];
        redeemers.push((
            RedeemersKey {
                tag: RedeemerTag::Cert,
                index: 0,
            },
            RedeemersValue {
                data: rules,
                ex_units: ex_units[1],
            },
        ));

        // ----- Put it all together
        let redeemers = NonEmptyKeyValuePairs::Def(redeemers);
        Tx {
            transaction_body: new_transaction_body(
                network.network_id(),
                inputs,
                vec![],
                outputs,
                Some(mint),
                certificates,
                vec![],
                (vec![fuel.clone()], collateral_return, total_collateral),
                fee,
                administrators.clone(),
                script_integrity_hash(
                    Some(&redeemers),
                    None,
                    &[(Language::PlutusV3, &params.cost_model_v3[..])],
                )
                .unwrap(),
            ),
            transaction_witness_set: new_witness_set(redeemers, validator.clone()),
            success: true,
            auxiliary_data: Nullable::Null,
        }
    })
}

async fn redelegate(
    network: Cardano,
    validator: Bytes,
    administrators: Vec<Hash<28>>,
    delegates: Vec<Hash<28>>,
    quorum: usize,
    OutputReference(contract): OutputReference,
    OutputReference(fuel): OutputReference,
) -> Result<Tx, Error> {
    let (validator_hash, validator_address) =
        from_validator(validator.as_ref(), network.network_id());

    let params = network.protocol_parameters().await;

    let contract_old_output = network
        .resolve(&contract)
        .await
        .expect("failed to resolve contract UTxO");

    let fuel_output = network
        .resolve(&fuel)
        .await
        .expect("failed to resolve fuel UTxO");

    let resolved_inputs = &[
        ResolvedInput {
            input: contract.clone(),
            output: PseudoTransactionOutput::PostAlonzo(contract_old_output.clone()),
        },
        ResolvedInput {
            input: fuel.clone(),
            output: PseudoTransactionOutput::PostAlonzo(fuel_output.clone()),
        },
    ];

    build_transaction(&params, resolved_inputs, |fee, ex_units| {
        let (rules, new_asset_name) = build_rules(&delegates[..], quorum);

        let old_asset_name = find_contract_token(&contract_old_output.value)
            .expect("no state token in contract utxo?");

        let contract_new_output =
            new_min_value_output(params.min_utxo_deposit_coefficient, |lovelace| {
                PostAlonzoTransactionOutput {
                    address: validator_address.to_vec().into(),
                    value: Value::Multiasset(
                        lovelace,
                        singleton_assets(
                            validator_hash,
                            &[(new_asset_name.clone(), PositiveCoin::try_from(1).unwrap())],
                        ),
                    ),
                    datum_option: None,
                    script_ref: None,
                }
            });

        let total_collateral = (fee as f64 * params.collateral_percent).ceil() as u64;

        let mut redeemers = vec![];

        let mut inputs = vec![contract.clone(), fuel.clone()];
        inputs.sort();

        let total_cost =
            lovelace_of(&contract_new_output.value) + fee - lovelace_of(&contract_old_output.value);

        let mint = singleton_assets(
            validator_hash,
            &[
                (new_asset_name, NonZeroInt::try_from(1).unwrap()),
                (old_asset_name, NonZeroInt::try_from(-1).unwrap()),
            ],
        );
        redeemers.push((
            RedeemersKey {
                tag: RedeemerTag::Mint,
                index: 0,
            },
            RedeemersValue {
                data: void(),
                ex_units: ex_units[0],
            },
        ));

        let outputs = vec![
            // Contract
            contract_new_output,
            // Change
            PostAlonzoTransactionOutput {
                address: fuel_output.address.clone(),
                value: subtract(fuel_output.value.clone(), total_cost).expect("not enough fuel"),
                datum_option: None,
                script_ref: None,
            },
        ];

        let collateral_return = PostAlonzoTransactionOutput {
            address: fuel_output.address.clone(),
            value: subtract(fuel_output.value.clone(), total_collateral).expect("not enough fuel"),
            datum_option: None,
            script_ref: None,
        };

        redeemers.push((
            RedeemersKey {
                tag: RedeemerTag::Spend,
                index: inputs
                    .iter()
                    .enumerate()
                    .find(|(_, i)| *i == &contract)
                    .unwrap()
                    .0 as u32,
            },
            RedeemersValue {
                data: void(),
                ex_units: ex_units[1],
            },
        ));

        let certificates = vec![
            Certificate::UnRegDRepCert(
                StakeCredential::Scripthash(validator_hash),
                params.drep_deposit,
            ),
            Certificate::RegDRepCert(
                StakeCredential::Scripthash(validator_hash),
                params.drep_deposit,
                Nullable::Null,
            ),
        ];
        redeemers.push((
            RedeemersKey {
                tag: RedeemerTag::Cert,
                index: 0,
            },
            RedeemersValue {
                data: void(),
                ex_units: ex_units[2],
            },
        ));
        redeemers.push((
            RedeemersKey {
                tag: RedeemerTag::Cert,
                index: 1,
            },
            RedeemersValue {
                data: rules,
                ex_units: ex_units[3],
            },
        ));

        // ----- Put it all together
        let redeemers = NonEmptyKeyValuePairs::Def(redeemers);
        Tx {
            transaction_body: new_transaction_body(
                network.network_id(),
                inputs,
                vec![],
                outputs,
                Some(mint),
                certificates,
                vec![],
                (vec![fuel.clone()], collateral_return, total_collateral),
                fee,
                administrators.clone(),
                script_integrity_hash(
                    Some(&redeemers),
                    None,
                    &[(Language::PlutusV3, &params.cost_model_v3[..])],
                )
                .unwrap(),
            ),
            transaction_witness_set: new_witness_set(redeemers, validator.clone()),
            success: true,
            auxiliary_data: Nullable::Null,
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn vote(
    network: Cardano,
    validator: Bytes,
    delegates: Vec<Hash<28>>,
    choice: Vote,
    anchor: Option<&str>,
    proposal_id: GovActionId,
    OutputReference(contract): OutputReference,
    OutputReference(fuel): OutputReference,
) -> Result<Tx, Error> {
    let (validator_hash, _) = from_validator(validator.as_ref(), network.network_id());

    let params = network.protocol_parameters().await;

    let contract_output = network
        .resolve(&contract)
        .await
        .expect("failed to resolve contract UTxO");

    let fuel_output = network
        .resolve(&fuel)
        .await
        .expect("failed to resolve fuel UTxO");

    let resolved_inputs = &[
        ResolvedInput {
            input: contract.clone(),
            output: PseudoTransactionOutput::PostAlonzo(contract_output.clone()),
        },
        ResolvedInput {
            input: fuel.clone(),
            output: PseudoTransactionOutput::PostAlonzo(fuel_output.clone()),
        },
    ];

    let (rules, _) = recover_rules(&network, &validator_hash, &contract_output.value).await;

    let anchor = if let Some(url) = anchor {
        let response = reqwest::get(url)
            .await
            .expect("failed to fetch anchor at URL: {url}");
        match response.status() {
            status if status.is_success() => {
                let content_hash = Hasher::<256>::hash(response.bytes().await.unwrap().as_ref());
                Some(Anchor {
                    url: url.to_string(),
                    content_hash,
                })
            }
            status => panic!("failed to fetch anchor content, server said: {status:?}"),
        }
    } else {
        None
    };

    build_transaction(&params, resolved_inputs, |fee, ex_units| {
        let mut redeemers = vec![];

        let inputs = vec![fuel.clone()];

        let reference_inputs = vec![contract.clone()];

        let outputs = vec![
            // Change
            PostAlonzoTransactionOutput {
                address: fuel_output.address.clone(),
                value: subtract(fuel_output.value.clone(), fee).expect("not enough fuel"),
                datum_option: None,
                script_ref: None,
            },
        ];

        let total_collateral = (fee as f64 * params.collateral_percent).ceil() as u64;

        let collateral_return = PostAlonzoTransactionOutput {
            address: fuel_output.address.clone(),
            value: subtract(fuel_output.value.clone(), total_collateral).expect("not enough fuel"),
            datum_option: None,
            script_ref: None,
        };

        let votes = vec![(
            Voter::DRepScript(validator_hash),
            NonEmptyKeyValuePairs::Def(vec![(
                proposal_id.clone(),
                VotingProcedure {
                    vote: choice.clone(),
                    anchor: anchor.clone().map(Nullable::Some).unwrap_or(Nullable::Null),
                },
            )]),
        )];
        redeemers.push((
            RedeemersKey {
                tag: RedeemerTag::Vote,
                index: 0,
            },
            RedeemersValue {
                data: rules.clone(),
                ex_units: ex_units[0],
            },
        ));

        // ----- Put it all together
        let redeemers = NonEmptyKeyValuePairs::Def(redeemers);
        Tx {
            transaction_body: new_transaction_body(
                network.network_id(),
                inputs,
                reference_inputs,
                outputs,
                None,
                vec![],
                votes,
                (vec![fuel.clone()], collateral_return, total_collateral),
                fee,
                delegates.clone(),
                script_integrity_hash(
                    Some(&redeemers),
                    None,
                    &[(Language::PlutusV3, &params.cost_model_v3[..])],
                )
                .unwrap(),
            ),
            transaction_witness_set: new_witness_set(redeemers, validator.clone()),
            success: true,
            auxiliary_data: Nullable::Null,
        }
    })
}

// Build a transaction by repeatedly executing some building logic with different fee and execution
// units settings. Stops when a fixed point is reached. The final transaction has corresponding
// fees and execution units.
fn build_transaction<E, F>(
    params: &ProtocolParameters,
    resolved_inputs: &[ResolvedInput],
    with: F,
) -> Result<Tx, E>
where
    F: Fn(u64, &[ExUnits]) -> Tx,
{
    let empty_ex_units = || {
        vec![
            ExUnits { mem: 0, steps: 0 },
            ExUnits { mem: 0, steps: 0 },
            ExUnits { mem: 0, steps: 0 },
            ExUnits { mem: 0, steps: 0 },
        ]
    };

    let mut fee = 0;
    let mut ex_units = empty_ex_units();

    let mut tx;
    let mut attempts = 0;
    loop {
        tx = with(fee, &ex_units[..]);

        // Convert to minted_tx...
        let mut serialized_tx = Vec::new();
        cbor::encode(&tx, &mut serialized_tx).unwrap();

        let mut calculated_ex_units = if resolved_inputs.is_empty() {
            empty_ex_units()
        } else {
            // Compute execution units
            let minted_tx = cbor::decode(&serialized_tx).unwrap();
            eval_phase_two(
                &minted_tx,
                resolved_inputs,
                None,
                None,
                &SlotConfig::default(),
                false,
                |_| (),
            )
            .expect("script evaluation failed")
            .into_iter()
            .map(|r| r.ex_units)
            .collect::<Vec<_>>()
        };

        calculated_ex_units.extend(empty_ex_units());

        attempts += 1;

        let estimated_fee = {
            // NOTE: This is a best effort to estimate the number of signatories since signatures
            // will add an overhead to the fee. Yet, if inputs are locked by native scripts each
            // requiring multiple signatories, this will unfortunately fall short.
            //
            // For similar reasons, it will also over-estimate fees by a small margin for every
            // script-locked inputs that do not require signatories.
            //
            // This is however *acceptable* in our context.
            let num_signatories = tx.transaction_body.inputs.len()
                + tx.transaction_body
                    .required_signers
                    .as_ref()
                    .map(|xs| xs.len())
                    .unwrap_or(0);

            params.fee_constant
                + params.fee_coefficient
                    * (5 + ex_units.len() * 16 + num_signatories * 102 + serialized_tx.len()) as u64
                + total_execution_cost(params, &ex_units)
        };

        // Check if we've reached a fixed point, or start over.
        if fee >= estimated_fee
            && calculated_ex_units
                .iter()
                .zip(ex_units)
                .all(|(l, r)| l.eq(&r))
        {
            break;
        } else if attempts >= 3 {
            panic!("failed to build transaction: did not converge after three attempts.");
        } else {
            ex_units = calculated_ex_units;
            fee = estimated_fee;
        }
    }

    Ok(tx)
}

// ---------------------------------------------------------------- helpers ----

fn default_transaction_body() -> TransactionBody {
    TransactionBody {
        auxiliary_data_hash: None,
        certificates: None,
        collateral: None,
        collateral_return: None,
        donation: None,
        fee: 0,
        inputs: Set::from(vec![]),
        mint: None,
        network_id: None,
        outputs: vec![],
        proposal_procedures: None,
        reference_inputs: None,
        required_signers: None,
        script_data_hash: None,
        total_collateral: None,
        treasury_value: None,
        ttl: None,
        validity_interval_start: None,
        voting_procedures: None,
        withdrawals: None,
    }
}

fn default_witness_set() -> WitnessSet {
    WitnessSet {
        bootstrap_witness: None,
        native_script: None,
        plutus_data: None,
        plutus_v1_script: None,
        plutus_v2_script: None,
        plutus_v3_script: None,
        redeemer: None,
        vkeywitness: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn new_transaction_body(
    network_id: Network,
    inputs: Vec<TransactionInput>,
    reference_inputs: Vec<TransactionInput>,
    outputs: Vec<PostAlonzoTransactionOutput>,
    mint: Option<Multiasset<NonZeroInt>>,
    certificates: Vec<Certificate>,
    votes: Vec<(Voter, NonEmptyKeyValuePairs<GovActionId, VotingProcedure>)>,
    (collateral, collateral_return, total_collateral): (
        Vec<TransactionInput>,
        PostAlonzoTransactionOutput,
        u64,
    ),
    fee: u64,
    extra_signatories: Vec<Hash<28>>,
    script_data_hash: Hash<32>,
) -> TransactionBody {
    TransactionBody {
        inputs: Set::from(inputs),
        reference_inputs: if reference_inputs.is_empty() {
            None
        } else {
            Some(NonEmptySet::try_from(reference_inputs).unwrap())
        },
        outputs: outputs
            .into_iter()
            .map(PseudoTransactionOutput::PostAlonzo)
            .collect(),
        fee,
        required_signers: NonEmptySet::try_from(extra_signatories).ok(),
        mint,
        certificates: if certificates.is_empty() {
            None
        } else {
            Some(NonEmptySet::try_from(certificates).unwrap())
        },
        voting_procedures: if votes.is_empty() {
            None
        } else {
            Some(NonEmptyKeyValuePairs::try_from(votes).unwrap())
        },
        collateral: Some(NonEmptySet::try_from(collateral).unwrap()),
        collateral_return: Some(PseudoTransactionOutput::PostAlonzo(collateral_return)),
        total_collateral: Some(total_collateral),
        network_id: Some(match network_id {
            Network::Mainnet => NetworkId::Two,
            _ => NetworkId::One,
        }),
        script_data_hash: Some(script_data_hash),
        ..default_transaction_body()
    }
}

fn new_witness_set(
    redeemers: NonEmptyKeyValuePairs<RedeemersKey, RedeemersValue>,
    validator: Bytes,
) -> WitnessSet {
    WitnessSet {
        redeemer: Some(redeemers.into()),
        plutus_v3_script: Some(NonEmptySet::try_from(vec![PlutusV3Script(validator)]).unwrap()),
        ..default_witness_set()
    }
}

fn void() -> PlutusData {
    PlutusData::Constr(Constr {
        tag: 121,
        any_constructor: None,
        fields: vec![],
    })
}

fn find_contract_token(value: &Value) -> Option<AssetName> {
    match value {
        Value::Multiasset(_, ref assets) => assets
            .first()
            .and_then(|(_, assets)| assets.first().cloned()),
        _ => None,
    }
    .map(|pair| pair.0)
}

fn build_rules(delegates: &[Hash<28>], quorum: usize) -> (PlutusData, AssetName) {
    assert!(
        quorum <= delegates.len(),
        "quorum cannot be larger than number of delegates"
    );

    assert!(!delegates.is_empty(), "there must be at least one delegate");

    let rules = PlutusData::Constr(Constr {
        tag: 123,
        any_constructor: None,
        fields: vec![PlutusData::Array(
            delegates
                .iter()
                .map(|delegate| {
                    PlutusData::Constr(Constr {
                        tag: 121,
                        any_constructor: None,
                        fields: vec![PlutusData::BoundedBytes(
                            delegate.as_slice().to_vec().into(),
                        )],
                    })
                })
                .collect::<Vec<_>>(),
        )],
    });

    let mut asset_name = "gov_".as_bytes().to_vec();
    asset_name.extend(Hasher::<224>::hash_cbor(&rules).as_slice());

    (rules, asset_name.into())
}

// To avoid re-asking users for the delegates and quorum during vote (which is (1) inconvenient,
// and (2), utterly confusing with the existing delegates signatories...), we pull the rules from
// the minting transaction corresponding to the current state token. The token is always minted
// alongside a DRep registration certificate which defines the new rules as redeemer.
async fn recover_rules(
    network: &Cardano,
    validator_hash: &Hash<28>,
    contract_value: &Value,
) -> (PlutusData, AssetName) {
    let asset_name = find_contract_token(contract_value).expect("no state token in contract utxo?");

    let minting_txs = network.minting(validator_hash, &asset_name).await;

    let minting_tx = minting_txs.first().unwrap_or_else(|| {
        panic!(
            "no minting transaction found for {}",
            hex::encode(&asset_name[..]),
        )
    });

    let rules = if let Some(ref redeemers) = minting_tx.transaction_witness_set.redeemer {
        redeemers
            .iter()
            .find_map(|(key, value)| {
                if key.tag == RedeemerTag::Cert && value.data != void() {
                    Some(value.data.clone())
                } else {
                    None
                }
            })
            .expect("could not find registration certificate alongside minting transaction?!")
    } else {
        unreachable!()
    };

    (rules, asset_name)
}

fn singleton_assets<T: Clone>(
    validator_hash: Hash<28>,
    assets: &[(AssetName, T)],
) -> Multiasset<T> {
    NonEmptyKeyValuePairs::Def(vec![(
        validator_hash,
        NonEmptyKeyValuePairs::Def(assets.to_vec()),
    )])
}

fn from_validator(validator: &[u8], network_id: Network) -> (Hash<28>, ShelleyAddress) {
    let validator_hash = Hasher::<224>::hash_tagged(validator, 3);
    let validator_address = ShelleyAddress::new(
        network_id,
        ShelleyPaymentPart::script_hash(validator_hash),
        ShelleyDelegationPart::script_hash(validator_hash),
    );

    (validator_hash, validator_address)
}

fn subtract(total_value: Value, total_cost: u64) -> Option<Value> {
    match total_value {
        Value::Coin(total) if total > total_cost => Some(Value::Coin(total - total_cost)),
        Value::Multiasset(total, assets) if total > total_cost => {
            Some(Value::Multiasset(total - total_cost, assets))
        }
        _ => None,
    }
}

fn lovelace_of(value: &Value) -> u64 {
    match value {
        Value::Coin(lovelace) | Value::Multiasset(lovelace, _) => *lovelace,
    }
}

// Move to Pallas somewhere.
fn new_min_value_output<F>(per_byte: u64, build: F) -> PostAlonzoTransactionOutput
where
    F: Fn(u64) -> PostAlonzoTransactionOutput,
{
    let value = build(1);
    let mut buffer: Vec<u8> = Vec::new();
    cbor::encode(&value, &mut buffer).unwrap();
    // NOTE: 160 overhead as per the spec + 4 bytes for actual final lovelace value.
    // Technically, the final value could need 8 more additional bytes if the resulting
    // value was larger than 4_294_967_295 lovelaces, which would realistically never be
    // the case.
    build((buffer.len() as u64 + 164) * per_byte)
}

fn total_execution_cost(params: &ProtocolParameters, redeemers: &[ExUnits]) -> u64 {
    redeemers.iter().fold(0, |acc, ex_units| {
        acc + ((params.price_mem * ex_units.mem as f64).ceil() as u64)
            + ((params.price_steps * ex_units.steps as f64).ceil() as u64)
    })
}

fn script_integrity_hash(
    redeemers: Option<&NonEmptyKeyValuePairs<RedeemersKey, RedeemersValue>>,
    datums: Option<&NonEmptyKeyValuePairs<Hash<32>, PlutusData>>,
    language_views: &[(Language, &[i64])],
) -> Option<Hash<32>> {
    if redeemers.is_none() && language_views.is_empty() && datums.is_none() {
        return None;
    }

    let mut preimage: Vec<u8> = Vec::new();
    if let Some(redeemers) = redeemers {
        cbor::encode(redeemers, &mut preimage).unwrap();
    }

    if let Some(datums) = datums {
        cbor::encode(datums, &mut preimage).unwrap();
    }

    // NOTE: This doesn't work for PlutusV1, but I don't care.
    if !language_views.is_empty() {
        let mut views = language_views.to_vec();
        // TODO: Derive an Ord instance in Pallas.
        views.sort_by(|(a, _), (b, _)| match (a, b) {
            (Language::PlutusV3, Language::PlutusV3) => Ordering::Equal,
            (Language::PlutusV3, _) => Ordering::Greater,
            (_, Language::PlutusV3) => Ordering::Less,

            (Language::PlutusV2, Language::PlutusV2) => Ordering::Equal,
            (Language::PlutusV2, _) => Ordering::Greater,
            (_, Language::PlutusV2) => Ordering::Less,

            (Language::PlutusV1, Language::PlutusV1) => Ordering::Equal,
        });
        cbor::encode(NonEmptyKeyValuePairs::Def(views), &mut preimage).unwrap()
    }

    Some(Hasher::<256>::hash(&preimage))
}
