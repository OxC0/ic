use candid::{candid_method, CandidType, Principal};
use candid::types::number::Nat;
use ic_canister_log::{declare_log_buffer, export};
use ic_canisters_http_types::{HttpRequest, HttpResponse, HttpResponseBuilder};
use ic_cdk::api::stable::{StableReader, StableWriter};
use ic_cdk::print;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade, query, update};
use ic_icrc1::{
    endpoints::{convert_transfer_error, StandardRecord},
    Operation, Transaction,
};
use ic_icrc1_ledger::{FeeInfo, Ledger, LedgerArgument};
use ic_icrc1_tokens_u256::U256;
use ic_ledger_canister_core::ledger::{
    apply_transaction, archive_blocks, LedgerAccess, LedgerContext, LedgerData,
    TransferError as CoreTransferError,
};
use ic_ledger_core::balances::{self, Balances};
use ic_ledger_core::tokens::{CheckedAdd, Zero};
use ic_ledger_core::{approvals::Approvals, timestamp::TimeStamp};
use icrc_ledger_types::icrc1::transfer::Memo;
use icrc_ledger_types::icrc2::approve::{ApproveArgs, ApproveError,ApproveError::GenericError};
use icrc_ledger_types::icrc3::blocks::DataCertificate;
use icrc_ledger_types::{
    icrc::generic_metadata_value::MetadataValue as Value,
    icrc3::{
        archive::ArchiveInfo,
        blocks::{GetBlocksRequest, GetBlocksResponse},
        transactions::{GetTransactionsRequest, GetTransactionsResponse},
    },
};
use icrc_ledger_types::{
    icrc1::account::Account,
    icrc2::allowance::{Allowance, AllowanceArgs},
};
use icrc_ledger_types::{
    icrc1::transfer::{TransferArg, TransferError},
    icrc2::transfer_from::{TransferFromArgs, TransferFromError},
};
use num_traits::{bounds::Bounded, ToPrimitive};
use serde_bytes::ByteBuf;
use std::borrow::BorrowMut;
use std::cell::RefCell;
use std::collections::{BTreeMap, LinkedList};
use std::fmt::format;
use std::ops::{Add, Div, Mul};
const MAX_MESSAGE_SIZE: u64 = 1024 * 1024;


type Tokens = ic_icrc1_tokens_u256::U256;

thread_local! {
    static LEDGER: RefCell<Option<Ledger<Tokens>>> = RefCell::new(None);
}

declare_log_buffer!(name = LOG, capacity = 1000);

struct Access;
impl LedgerAccess for Access {
    type Ledger = Ledger<Tokens>;

    fn with_ledger<R>(f: impl FnOnce(&Self::Ledger) -> R) -> R {
        LEDGER.with(|cell| {
            f(cell
                .borrow()
                .as_ref()
                .expect("ledger state not initialized"))
        })
    }

    fn with_ledger_mut<R>(f: impl FnOnce(&mut Self::Ledger) -> R) -> R {
        LEDGER.with(|cell| {
            f(cell
                .borrow_mut()
                .as_mut()
                .expect("ledger state not initialized"))
        })
    }
}

#[candid_method(init)]
#[init]
fn init(args: LedgerArgument) {
    match args {
        LedgerArgument::Init(init_args) => {
            let now = TimeStamp::from_nanos_since_unix_epoch(ic_cdk::api::time());
            LEDGER.with(|cell| {
                *cell.borrow_mut() = Some(Ledger::<Tokens>::from_init_args(&LOG, init_args, now))
            })
        }
        LedgerArgument::Upgrade(_) => {
            panic!("Cannot initialize the canister with an Upgrade argument. Please provide an Init argument.");
        }
    }
    ic_cdk::api::set_certified_data(&Access::with_ledger(Ledger::root_hash));
}

#[pre_upgrade]
fn pre_upgrade() {
    Access::with_ledger(|ledger| ciborium::ser::into_writer(ledger, StableWriter::default()))
        .expect("failed to encode ledger state");
}

#[post_upgrade]
fn post_upgrade(args: Option<LedgerArgument>) {
    LEDGER.with(|cell| {
        *cell.borrow_mut() = Some(
            ciborium::de::from_reader(StableReader::default())
                .expect("failed to decode ledger state"),
        );
    });

    if let Some(args) = args {
        match args {
            LedgerArgument::Init(_) => panic!("Cannot upgrade the canister with an Init argument. Please provide an Upgrade argument."),
            LedgerArgument::Upgrade(upgrade_args) => {
                if let Some(upgrade_args) = upgrade_args {
                    Access::with_ledger_mut(|ledger| ledger.upgrade(&LOG, upgrade_args));
                }
            }
        }
    }
}

fn encode_metrics(w: &mut ic_metrics_encoder::MetricsEncoder<Vec<u8>>) -> std::io::Result<()> {
    w.encode_gauge(
        "ledger_stable_memory_pages",
        ic_cdk::api::stable::stable64_size() as f64,
        "Size of the stable memory allocated by this canister measured in 64K Wasm pages.",
    )?;
    w.encode_gauge(
        "ledger_stable_memory_bytes",
        (ic_cdk::api::stable::stable64_size() * 64 * 1024) as f64,
        "Size of the stable memory allocated by this canister.",
    )?;

    let cycle_balance = ic_cdk::api::canister_balance128() as f64;
    w.encode_gauge(
        "ledger_cycle_balance",
        cycle_balance,
        "Cycle balance on the ledger canister.",
    )?;
    w.gauge_vec("cycle_balance", "Cycle balance on the ledger canister.")?
        .value(&[("canister", "icrc1-ledger")], cycle_balance)?;

    Access::with_ledger(|ledger| {
        w.encode_gauge(
            "ledger_transactions_by_hash_cache_entries",
            ledger.transactions_by_hash().len() as f64,
            "Total number of entries in the transactions_by_hash cache.",
        )?;
        w.encode_gauge(
            "ledger_transactions_by_height_entries",
            ledger.transactions_by_height().len() as f64,
            "Total number of entries in the transaction_by_height queue.",
        )?;
        w.encode_gauge(
            "ledger_transactions",
            ledger.blockchain().blocks.len() as f64,
            "Total number of transactions stored in the main memory.",
        )?;
        w.encode_gauge(
            "ledger_archived_transactions",
            ledger.blockchain().num_archived_blocks as f64,
            "Total number of transactions sent to the archive.",
        )?;
        let token_pool: Nat = ledger.balances().token_pool.into();
        w.encode_gauge(
            "ledger_balances_token_pool",
            token_pool.0.to_f64().unwrap_or(f64::INFINITY),
            "Total number of Tokens in the pool.",
        )?;
        let total_supply: Nat = ledger.balances().total_supply().into();
        w.encode_gauge(
            "ledger_total_supply",
            total_supply.0.to_f64().unwrap_or(f64::INFINITY),
            "Total number of tokens in circulation.",
        )?;
        w.encode_gauge(
            "ledger_balance_store_entries",
            ledger.balances().store.len() as f64,
            "Total number of accounts in the balance store.",
        )?;
        w.encode_gauge(
            "ledger_most_recent_block_time_seconds",
            (ledger
                .blockchain()
                .last_timestamp
                .as_nanos_since_unix_epoch()
                / 1_000_000_000) as f64,
            "IC timestamp of the most recent block.",
        )?;
        Ok(())
    })
}

#[query(hidden = true)]
fn http_request(req: HttpRequest) -> HttpResponse {
    if req.path() == "/metrics" {
        let mut writer =
            ic_metrics_encoder::MetricsEncoder::new(vec![], ic_cdk::api::time() as i64 / 1_000_000);

        match encode_metrics(&mut writer) {
            Ok(()) => HttpResponseBuilder::ok()
                .header("Content-Type", "text/plain; version=0.0.4")
                .with_body_and_content_length(writer.into_inner())
                .build(),
            Err(err) => {
                HttpResponseBuilder::server_error(format!("Failed to encode metrics: {}", err))
                    .build()
            }
        }
    } else if req.path() == "/logs" {
        use std::io::Write;
        let mut buf = vec![];
        for entry in export(&LOG) {
            writeln!(
                &mut buf,
                "{} {}:{} {}",
                entry.timestamp, entry.file, entry.line, entry.message
            )
            .unwrap();
        }
        HttpResponseBuilder::ok()
            .header("Content-Type", "text/plain; charset=utf-8")
            .with_body_and_content_length(buf)
            .build()
    } else {
        HttpResponseBuilder::not_found().build()
    }
}

#[query]
#[candid_method(query)]
fn icrc1_name() -> String {
    Access::with_ledger(|ledger| ledger.token_name().to_string())
}

#[query]
#[candid_method(query)]
fn icrc1_symbol() -> String {
    Access::with_ledger(|ledger| ledger.token_symbol().to_string())
}

#[query]
#[candid_method(query)]
fn icrc1_decimals() -> u8 {
    Access::with_ledger(|ledger| ledger.decimals())
}

#[query]
#[candid_method(query)]
fn icrc1_fee() -> Nat {
    Access::with_ledger(|ledger| ledger.transfer_fee().into())
}

#[query]
#[candid_method(query)]
fn icrc1_metadata() -> Vec<(String, Value)> {
    Access::with_ledger(|ledger| ledger.metadata())
}

#[query]
#[candid_method(query)]
fn icrc1_minting_account() -> Option<Account> {
    Access::with_ledger(|ledger| Some(*ledger.minting_account()))
}

#[query(name = "icrc1_balance_of")]
#[candid_method(query, rename = "icrc1_balance_of")]
fn icrc1_balance_of(account: Account) -> Nat {
    Access::with_ledger(|ledger| ledger.balances().account_balance(&account).into())
}

#[query(name = "icrc1_total_supply")]
#[candid_method(query, rename = "icrc1_total_supply")]
fn icrc1_total_supply() -> Nat {
    Access::with_ledger(|ledger| ledger.balances().total_supply().into())
}

async fn execute_transfer(
    from_account: Account,
    to: Account,
    spender: Option<Account>,
    fee: Option<Nat>,
    amount: Nat,
    memo: Option<Memo>,
    created_at_time: Option<u64>,
) -> Result<Nat, CoreTransferError<Tokens>> {
    let block_idx = Access::with_ledger_mut(|ledger| {
        let now = TimeStamp::from_nanos_since_unix_epoch(ic_cdk::api::time());
        let created_at_time = created_at_time.map(TimeStamp::from_nanos_since_unix_epoch);
        let memo_burn = memo.clone();
        match memo.as_ref() {
            Some(memo) if memo.0.len() > ledger.max_memo_length() as usize => {
                ic_cdk::trap(&format!(
                    "the memo field size of {} bytes is above the allowed limit of {} bytes",
                    memo.0.len(),
                    ledger.max_memo_length()
                ))
            }
            _ => {}
        };
        let amount = match Tokens::try_from(amount.clone()) {
            Ok(n) => n,
            Err(_) => {
                // No one can have so many tokens
                let balance_tokens = ledger.balances().account_balance(&from_account);
                let balance = Nat::from(balance_tokens);
                assert!(balance < amount);
                return Err(CoreTransferError::InsufficientFunds {
                    balance: balance_tokens,
                });
            }
        };

        let (tx, effective_fee) = if &to == ledger.minting_account() {
            let expected_fee = Tokens::zero();
            if fee.is_some() && fee.as_ref() != Some(&expected_fee.into()) {
                return Err(CoreTransferError::BadFee { expected_fee });
            }

            let balance = ledger.balances().account_balance(&from_account);
            let min_burn_amount = ledger.transfer_fee().min(balance);
            if amount < min_burn_amount {
                return Err(CoreTransferError::BadBurn { min_burn_amount });
            }
            if Tokens::is_zero(&amount) {
                return Err(CoreTransferError::BadBurn {
                    min_burn_amount: ledger.transfer_fee(),
                });
            }

            (
                Transaction {
                    operation: Operation::Burn {
                        from: from_account,
                        spender,
                        amount,
                    },
                    created_at_time: created_at_time.map(|t| t.as_nanos_since_unix_epoch()),
                    memo,
                },
                Tokens::zero(),
            )
        } else if &from_account == ledger.minting_account() {
            if spender.is_some() {
                ic_cdk::trap("the minter account cannot delegate mints")
            }
            let expected_fee = Tokens::zero();
            if fee.is_some() && fee.as_ref() != Some(&expected_fee.into()) {
                return Err(CoreTransferError::BadFee { expected_fee });
            }
            (
                Transaction::mint(to, amount, created_at_time, memo),
                Tokens::zero(),
            )
        } else {
            let mut expected_fee_tokens = ledger.transfer_fee();
            if fee.is_some() && fee.as_ref() != Some(&expected_fee_tokens.into()) {
                return Err(CoreTransferError::BadFee {
                    expected_fee: expected_fee_tokens,
                });
            }
            ic_cdk::print(format!("rate fee1:{},nat amount:{}，muti:{}",Nat::from(ledger.transfer_fee_rate()),Nat::from(amount),Nat::from(amount).mul(Nat::from(ledger.transfer_fee_rate()))));
            if ledger.transfer_fee_rate() != Tokens::zero() {
                //transfer fee rate prefer
                let fee_amount = Nat::from(amount).mul(Nat::from(ledger.transfer_fee_rate())).div(Nat::from(10_000u32));
                ic_cdk::print(format!("rate fee2:{}",Nat::from(fee_amount.clone())));
                expected_fee_tokens = Tokens::try_from(fee_amount).unwrap_or_else(|_| panic!("bas transfer fee Rate:{}",ledger.transfer_fee_rate()));
            }
            (
                Transaction::transfer(
                    from_account,
                    to,
                    spender,
                    amount,
                    fee.map(|_| expected_fee_tokens),
                    created_at_time,
                    memo,
                ),
                expected_fee_tokens,
            )
        };

        if  &from_account != ledger.minting_account() && &to != ledger.minting_account(){
            let mut burn_fee = ledger.burn_fee().into();
            ic_cdk::print(format!("before:{}，burn_fee_rate:{},muti:{}",Nat::from(burn_fee),Nat::from(ledger.burn_fee_rate()),Nat::from(amount).mul(Nat::from(ledger.burn_fee_rate()))));
            if ledger.burn_fee_rate() != Tokens::zero() {
                let fee_nat = Nat::from(amount).mul(Nat::from(ledger.burn_fee_rate())).div(Nat::from(10_000u32));
                ic_cdk::print(format!("fee nat:{}",fee_nat));
                burn_fee = Tokens::try_from(fee_nat).unwrap_or_else(|_| panic!("Bas burn fee rate:{}",ledger.burn_fee_rate()));
            }
            ic_cdk::print(format!("after:{}",Nat::from(burn_fee)));
            if burn_fee != Tokens::zero(){
                let to_account = Account {
                    owner: Principal::management_canister(),
                    subaccount: None,
                };
                let burn_tx = Transaction::transfer(
                    from_account,
                    to_account,
                    spender,
                    burn_fee,
                    Some(Tokens::zero()),
                    created_at_time,
                    memo_burn,
                );
                let total_amount =Nat::from(effective_fee)+Nat::from(burn_fee)+Nat::from(amount);
                let balance = Nat::from(ledger.balances().account_balance(&from_account));
                if total_amount > balance {
                    let balance_tokens = ledger.balances().account_balance(&from_account);
                    return Err(CoreTransferError::InsufficientFunds {
                        balance: balance_tokens,
                    })
                };
                apply_transaction(ledger, burn_tx, now, Tokens::zero())?;
            }
        }
        let (block_idx, _) = apply_transaction(ledger, tx, now, effective_fee)?;
        Ok(block_idx)
    })?;

    // NB. we need to set the certified data before the first async call to make sure that the
    // blockchain state agrees with the certificate while archiving is in progress.
    ic_cdk::api::set_certified_data(&Access::with_ledger(Ledger::root_hash));

    archive_blocks::<Access>(&LOG, MAX_MESSAGE_SIZE).await;
    Ok(Nat::from(block_idx))
}

#[update]
#[candid_method(update)]
async fn icrc1_transfer(arg: TransferArg) -> Result<Nat, TransferError> {
    let from_account = Account {
        owner: ic_cdk::api::caller(),
        subaccount: arg.from_subaccount,
    };
    execute_transfer(
        from_account,
        arg.to,
        None,
        arg.fee,
        arg.amount,
        arg.memo,
        arg.created_at_time,
    )
    .await
    .map_err(convert_transfer_error)
    .map_err(|err| {
        let err: TransferError = match err.try_into() {
            Ok(err) => err,
            Err(err) => ic_cdk::trap(&err),
        };
        err
    })
}

#[update]
#[candid_method(update)]
async fn icrc2_transfer_from(arg: TransferFromArgs) -> Result<Nat, TransferFromError> {
    let spender_account = Account {
        owner: ic_cdk::api::caller(),
        subaccount: arg.spender_subaccount,
    };
    execute_transfer(
        arg.from,
        arg.to,
        Some(spender_account),
        arg.fee,
        arg.amount,
        arg.memo,
        arg.created_at_time,
    )
    .await
    .map_err(convert_transfer_error)
    .map_err(|err| {
        let err: TransferFromError = match err.try_into() {
            Ok(err) => err,
            Err(err) => ic_cdk::trap(&err),
        };
        err
    })
}

#[query]
fn archives() -> Vec<ArchiveInfo> {
    Access::with_ledger(|ledger| {
        ledger
            .blockchain()
            .archive
            .read()
            .unwrap()
            .as_ref()
            .iter()
            .flat_map(|archive| {
                archive
                    .index()
                    .into_iter()
                    .map(|((start, end), canister_id)| ArchiveInfo {
                        canister_id: canister_id.get().0,
                        block_range_start: Nat::from(start),
                        block_range_end: Nat::from(end),
                    })
            })
            .collect()
    })
}

#[query(name = "icrc1_supported_standards")]
#[candid_method(query, rename = "icrc1_supported_standards")]
fn supported_standards() -> Vec<StandardRecord> {
    let standards = vec![
        StandardRecord {
            name: "ICRC-1".to_string(),
            url: "https://github.com/dfinity/ICRC-1/tree/main/standards/ICRC-1".to_string(),
        },
        StandardRecord {
            name: "ICRC-2".to_string(),
            url: "https://github.com/dfinity/ICRC-1/tree/main/standards/ICRC-2".to_string(),
        },
    ];
    standards
}

#[query]
#[candid_method(query)]
fn get_transactions(req: GetTransactionsRequest) -> GetTransactionsResponse {
    let (start, length) = req
        .as_start_and_length()
        .unwrap_or_else(|msg| ic_cdk::api::trap(&msg));
    Access::with_ledger(|ledger| ledger.get_transactions(start, length as usize))
}

#[query]
#[candid_method(query)]
fn get_blocks(req: GetBlocksRequest) -> GetBlocksResponse {
    let (start, length) = req
        .as_start_and_length()
        .unwrap_or_else(|msg| ic_cdk::api::trap(&msg));
    Access::with_ledger(|ledger| ledger.get_blocks(start, length as usize))
}

#[query]
#[candid_method(query)]
fn get_data_certificate() -> DataCertificate {
    let hash_tree = Access::with_ledger(|ledger| ledger.construct_hash_tree());
    let mut tree_buf = vec![];
    ciborium::ser::into_writer(&hash_tree, &mut tree_buf).unwrap();
    DataCertificate {
        certificate: ic_cdk::api::data_certificate().map(ByteBuf::from),
        hash_tree: ByteBuf::from(tree_buf),
    }
}

#[update]
#[candid_method(update)]
async fn icrc2_approve(arg: ApproveArgs) -> Result<Nat, ApproveError> {
    let block_idx = Access::with_ledger_mut(|ledger| {
        let now = TimeStamp::from_nanos_since_unix_epoch(ic_cdk::api::time());

        let from_account = Account {
            owner: ic_cdk::api::caller(),
            subaccount: arg.from_subaccount,
        };
        if from_account.owner == arg.spender.owner {
            ic_cdk::trap("self approval is not allowed")
        }
        if &from_account == ledger.minting_account() {
            ic_cdk::trap("the minting account cannot delegate mints")
        }
        match arg.memo.as_ref() {
            Some(memo) if memo.0.len() > ledger.max_memo_length() as usize => {
                ic_cdk::trap("the memo field is too large")
            }
            _ => {}
        };
        let amount = Tokens::try_from(arg.amount).unwrap_or_else(|_| Tokens::max_value());
        let expected_allowance = match arg.expected_allowance {
            Some(n) => match Tokens::try_from(n) {
                Ok(n) => Some(n),
                Err(_) => {
                    let current_allowance = ledger
                        .approvals()
                        .allowance(&from_account, &arg.spender, now)
                        .amount;
                    return Err(ApproveError::AllowanceChanged {
                        current_allowance: current_allowance.into(),
                    });
                }
            },
            None => None,
        };

        let expected_fee_tokens = ledger.transfer_fee();
        let expected_fee: Nat = expected_fee_tokens.into();
        if arg.fee.is_some() && arg.fee.as_ref() != Some(&expected_fee) {
            return Err(ApproveError::BadFee { expected_fee });
        }

        let tx = Transaction {
            operation: Operation::Approve {
                from: from_account,
                spender: arg.spender,
                amount,
                expected_allowance,
                expires_at: arg.expires_at,
                fee: arg.fee.map(|_| expected_fee_tokens),
            },
            created_at_time: arg.created_at_time,
            memo: arg.memo,
        };

        let (block_idx, _) = apply_transaction(ledger, tx, now, expected_fee_tokens)
            .map_err(convert_transfer_error)
            .map_err(|err| {
                let err: ApproveError = match err.try_into() {
                    Ok(err) => err,
                    Err(err) => ic_cdk::trap(&err),
                };
                err
            })?;
        Ok(block_idx)
    })?;

    // NB. we need to set the certified data before the first async call to make sure that the
    // blockchain state agrees with the certificate while archiving is in progress.
    ic_cdk::api::set_certified_data(&Access::with_ledger(Ledger::root_hash));

    archive_blocks::<Access>(&LOG, MAX_MESSAGE_SIZE).await;
    Ok(Nat::from(block_idx))
}

#[update]
#[candid_method(update)]
async fn icrc_plus_set_minting_account(arg: Account) -> Result<(), String> {
    let account=Access::with_ledger(|ledger| *ledger.minting_account());
    let caller=ic_cdk::api::caller();
    if &caller == &account.owner {
        Access::with_ledger_mut(|ledger| {
            ledger.up_minting_account(arg);
        });
        return Ok(());
    }
    return Err("caller must minting account!".to_string());
}

#[derive(Clone, Debug,CandidType)]
struct HoldersBalances {
    max_length: Nat,
    balances: LinkedList<(Account, Nat)>,
}

#[query]
#[candid_method(query)]
fn icrc_plus_holders_balance(start: usize, limit: usize) -> HoldersBalances {
    Access::with_ledger(|ledger| {
        let now = TimeStamp::from_nanos_since_unix_epoch(ic_cdk::api::time());

        // Get balances_store from ledger
        let balances_store = ledger.balances().store.clone();

        // Count total number of non-zero balances
        let total_count: usize = balances_store.iter().count();

        // Convert start and limit to usize
        let start_index: usize = start;
        let limit_index: usize = limit;

        // Calculate max_length
        let max_length: Nat = (total_count as u64).into();

        // Filter, map, and collect with pagination
        let paginated_balances = balances_store
            .iter()
            .enumerate()
            .filter(|(index, (_, value))| {
                // Check if index is within the pagination range
                *index >= start_index && *index < start_index + limit_index
            })
            .map(|(_, (account, value))| (account.clone(), value.clone().into()))
            .collect();
        HoldersBalances {
            max_length,
            balances: paginated_balances,
        }
    })
}

#[query]
#[candid_method(query)]
fn icrc2_allowance(arg: AllowanceArgs) -> Allowance {
    Access::with_ledger(|ledger| {
        let now = TimeStamp::from_nanos_since_unix_epoch(ic_cdk::api::time());
        let allowance = ledger
            .approvals()
            .allowance(&arg.account, &arg.spender, now);
        Allowance {
            allowance: allowance.amount.into(),
            expires_at: allowance.expires_at.map(|t| t.as_nanos_since_unix_epoch()),
        }
    })
}

#[query]
#[candid_method(query)]
fn icrc_plus_cycles() -> Nat {
    Nat::from(ic_cdk::api::canister_balance())
}


#[query]
#[candid_method(query)]
fn icrc_plus_fee_info() -> FeeInfo{
    let transfer_fee = Access::with_ledger(|ledger| ledger.transfer_fee().into());
    let burn_fee = Access::with_ledger(|ledger| ledger.burn_fee().into());
    let decimals = Access::with_ledger(|ledger| ledger.decimals().into());
    FeeInfo { transfer_fee: transfer_fee, burn_fee: burn_fee, decimals: decimals}
}



#[query]
#[candid_method(query)]
fn icrc_plus_holders_count() -> Nat {
    Nat::from(Access::with_ledger(|ledger| ledger.balances().store.len() ))
}

#[cfg(any(target_arch = "wasm32", test))]
fn main() {}

#[cfg(not(any(target_arch = "wasm32", test)))]
fn main() {
    use std::env;
    use std::fs::write;
    use std::path::PathBuf;
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    write(dir.join("ic-icrc1-ledger.did"), export_candid()).expect("Write failed.");
}


fn export_candid() -> String {
    candid::export_service!();
    __export_service()
}
#[test]
fn check_candid_interface() {

}
