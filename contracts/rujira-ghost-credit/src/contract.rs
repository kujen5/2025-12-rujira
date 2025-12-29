use crate::config::Config;
use crate::error::ContractError;
use crate::events::{
    event_create_account, event_execute_account, event_execute_account_borrow,
    event_execute_account_execute, event_execute_account_repay, event_execute_account_send,
    event_execute_account_set_preference_msgs, event_execute_account_set_preference_order,
    event_execute_account_transfer, event_execute_liquidate, event_execute_liquidate_execute,
    event_execute_liquidate_preference_error, event_execute_liquidate_repay,
};
use crate::{account::CreditAccount, state::BORROW};
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    coin, coins, ensure_eq, from_json, to_json_binary, BankMsg, Binary, CosmosMsg, Deps, DepsMut,
    Env, Event, Fraction, MessageInfo, Reply, Response, StdError, SubMsg, SubMsgResult,
};
use cw2::set_contract_version;
use cw_utils::NativeBalance;
use rujira_rs::ghost;
use rujira_rs::ghost::credit::{
    AccountMsg, AccountResponse, AccountsResponse, ConfigResponse, ExecuteMsg, InstantiateMsg,
    LiquidateMsg, QueryMsg, SudoMsg,
};
use rujira_rs::ghost::vault::Vault;
use std::ops::Sub;

const CONTRACT_NAME: &str = env!("CARGO_PKG_NAME");
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

const REPLY_ID_PREFERENCE: u64 = 0;
const REPLY_ID_LIQUIDATOR: u64 = 1;

//Ok1st
//e set the contract version and name, load the configs from the msg and save them to the contract
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    let config = Config::from(msg);
    config.validate()?;
    config.save(deps.storage)?;
    Ok(Response::default())
}

//
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    let config = Config::load(deps.storage)?;
    let ca = env.contract.address.clone();
    match msg {
        ExecuteMsg::Create { salt, label, tag } => {
            let (account, msg) = CreditAccount::create(
                deps.as_ref(),
                config.code_id,
                ca,
                info.sender,
                label,
                tag,
                salt,
            )?;
            account.save(deps)?;

            Ok(Response::default()
                .add_message(msg)
                .add_event(event_create_account(&account)))
        }
        ExecuteMsg::Liquidate { addr, msgs } => {
            let account =
                CreditAccount::load(deps.as_ref(), &config, &ca, deps.api.addr_validate(&addr)?)?;
            account.check_unsafe(&config.liquidation_threshold)?;
            let mut queue: Vec<(LiquidateMsg, bool)> =
                msgs.iter().map(|x| (x.clone(), false)).collect();
            queue.reverse();
            let mut prefs: Vec<(LiquidateMsg, bool)> = account
                .liquidation_preferences
                .messages
                .iter()
                .map(|x| (x.clone(), true))
                .collect();
            prefs.reverse();
            queue.append(&mut prefs);

            Ok(Response::default()
                .add_message(
                    ExecuteMsg::DoLiquidate {
                        addr: account.id().to_string(),
                        queue,
                        payload: to_json_binary(&account)?,
                    }
                    .call(&ca)?,
                )
                .add_event(event_execute_liquidate(&account, &info.sender)))
        }
        ExecuteMsg::DoLiquidate {
            addr,
            mut queue,
            payload,
        } => {
            ensure_eq!(info.sender, ca, ContractError::Unauthorized {});
            let account =
                CreditAccount::load(deps.as_ref(), &config, &ca, deps.api.addr_validate(&addr)?)?;
            let original_account: CreditAccount = from_json(&payload)?;

            let check = account
                // Check safe against the liquidation threshold
                .check_safe(&config.liquidation_threshold)
                // Check we've not gone below the adjustment threshold
                .and_then(|_| account.check_unsafe(&config.adjustment_threshold))
                .and_then(|_| {
                    account.validate_liquidation(deps.as_ref(), &config, &original_account)
                });
            match (queue.pop(), check) {
                (_, Ok(())) => Ok(Response::default()),
                (None, Err(err)) => {
                    // We're done and the Account hasn't passed checks. Fail
                    Err(err)
                }
                (Some((msg, is_preference)), Err(_)) => {
                    // Not safe, more messages to go. Continue
                    Ok(execute_liquidate(
                        deps.as_ref(),
                        env.clone(),
                        info,
                        &config,
                        msg,
                        &account,
                        if is_preference {
                            REPLY_ID_PREFERENCE
                        } else {
                            REPLY_ID_LIQUIDATOR
                        },
                    )?
                    .add_message(
                        ExecuteMsg::DoLiquidate {
                            addr: account.id().to_string(),
                            queue,
                            payload,
                        }
                        .call(&ca)?,
                    ))
                }
            }
        }

        ExecuteMsg::Account { addr, msgs } => {
            let mut account =
                CreditAccount::load(deps.as_ref(), &config, &ca, deps.api.addr_validate(&addr)?)?;
            ensure_eq!(account.owner, info.sender, ContractError::Unauthorized {});
            let mut response = Response::default().add_event(event_execute_account(&account));
            for msg in msgs {
                let (messages, events) =
                    execute_account(deps.as_ref(), env.clone(), &config, msg, &mut account)?;
                response = response.add_messages(messages).add_events(events);
            }
            account.save(deps)?;

            Ok(response.add_message(ExecuteMsg::CheckAccount { addr }.call(&ca)?))
        }
        ExecuteMsg::CheckAccount { addr } => {
            let account =
                CreditAccount::load(deps.as_ref(), &config, &ca, deps.api.addr_validate(&addr)?)?;
            account.check_safe(&config.adjustment_threshold)?;
            Ok(Response::default())
        }
    }
}

pub fn execute_account(
    deps: Deps,
    env: Env,
    config: &Config,
    msg: AccountMsg,
    account: &mut CreditAccount,
) -> Result<(Vec<CosmosMsg>, Vec<Event>), ContractError> {
    let delegate = account.id().to_string();

    match msg {
        AccountMsg::Borrow(coin) => {
            let vault = BORROW.load(deps.storage, coin.denom.clone())?;
            let msgs = vec![
                vault.market_msg_borrow(Some(delegate.clone()), None, &coin)?,
                BankMsg::Send {
                    to_address: delegate,
                    amount: vec![coin.clone()],
                }
                .into(),
            ];
            Ok((msgs, vec![event_execute_account_borrow(&coin)]))
        }
        AccountMsg::Repay(coin) => {
            let vault = BORROW.load(deps.storage, coin.denom.clone())?;
            let msgs = vec![
                account
                    .account
                    .send(env.contract.address, vec![coin.clone()])?,
                vault.market_msg_repay(Some(delegate), &coin)?,
            ];
            Ok((msgs, vec![event_execute_account_repay(&coin)]))
        }
        AccountMsg::Execute {
            contract_addr,
            msg,
            funds,
        } => {
            let event =
                event_execute_account_execute(&contract_addr, &msg, &NativeBalance(funds.clone()));
            Ok((
                vec![account.account.execute(contract_addr, msg, funds)?],
                vec![event],
            ))
        }
        AccountMsg::Send { to_address, funds } => Ok((
            vec![account.account.send(&to_address, funds.clone())?],
            vec![event_execute_account_send(
                &to_address,
                &NativeBalance(funds),
            )],
        )),

        AccountMsg::Transfer(recipient) => {
            let recipient = deps.api.addr_validate(&recipient)?;
            account.owner = recipient.clone();
            Ok((vec![], vec![event_execute_account_transfer(&recipient)]))
        }
        AccountMsg::SetPreferenceOrder { denom, after } => {
            if !config.collateral_ratios.contains_key(&denom) {
                return Err(ContractError::InvalidCollateral { denom });
            }
            if let Some(after) = after.clone() {
                if !config.collateral_ratios.contains_key(&after) {
                    return Err(ContractError::InvalidCollateral { denom: after });
                }
            }
            account.set_preference_order(&denom, &after)?;
            Ok((
                vec![],
                vec![event_execute_account_set_preference_order(&denom, &after)],
            ))
        }
        AccountMsg::SetPreferenceMsgs(msgs) => {
            account.set_preference_msgs(msgs);
            Ok((vec![], vec![event_execute_account_set_preference_msgs()]))
        }
    }
}

pub fn execute_liquidate(
    deps: Deps,
    env: Env,
    info: MessageInfo,
    config: &Config,
    msg: LiquidateMsg,
    account: &CreditAccount,
    reply_id: u64,
) -> Result<Response, ContractError> {
    let delegate = account.id().to_string();

    match msg {
        LiquidateMsg::Repay(denom) => {
            let vault = BORROW.load(deps.storage, denom.clone())?;
            // We repay the full balance so that Repay can be chained in liquidation preferences messages
            // and still pass the no-over-liquidation check, as we can't know ahead of time the amount to repay
            // after a collateral swap
            let balance = deps.querier.query_balance(account.id(), &denom)?;

            if balance.amount.is_zero() {
                return Err(ContractError::ZeroDebtTokens {
                    denom: balance.denom,
                });
            }

            // Collect fees from the amount retrieved from the rujira-account.
            // A liquidation solver must ensure that the repayment is sufficient
            // after these fees are deducted
            let liquidation_fee = balance.amount.multiply_ratio(
                config.fee_liquidation.numerator(),
                config.fee_liquidation.denominator(),
            );
            let liquidator_fee = balance.amount.multiply_ratio(
                config.fee_liquidator.numerator(),
                config.fee_liquidator.denominator(),
            );

            let repay_amount = balance.amount.sub(liquidation_fee).sub(liquidator_fee);

            Ok(Response::default()
                .add_message(
                    account
                        .account
                        .send(env.contract.address.to_string(), vec![balance.clone()])?,
                )
                .add_message(
                    vault.market_msg_repay(
                        Some(delegate),
                        &coin(repay_amount.u128(), denom.clone()),
                    )?,
                )
                .add_message(BankMsg::Send {
                    to_address: config.fee_address.to_string(),
                    amount: coins(liquidation_fee.u128(), denom.clone()),
                })
                .add_message(BankMsg::Send {
                    to_address: info.sender.to_string(),
                    amount: coins(liquidator_fee.u128(), denom.clone()),
                })
                .add_event(event_execute_liquidate_repay(
                    &balance,
                    repay_amount,
                    liquidation_fee,
                    liquidator_fee,
                )))
        }
        LiquidateMsg::Execute {
            contract_addr,
            msg,
            funds,
        } => Ok(Response::default()
            .add_submessage(
                SubMsg::reply_always(
                    account
                        .account
                        .execute(contract_addr.clone(), msg.clone(), funds.clone())?,
                    reply_id,
                )
                .with_payload(to_json_binary(&account)?),
            )
            .add_event(event_execute_liquidate_execute(
                &contract_addr,
                &msg,
                &NativeBalance(funds),
            ))),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(_deps: DepsMut, _env: Env, msg: Reply) -> Result<Response, ContractError> {
    match (msg.result, msg.id) {
        (SubMsgResult::Err(err), REPLY_ID_PREFERENCE) => {
            // Don't block execution if this is a preferential step
            Ok(Response::default().add_event(event_execute_liquidate_preference_error(err)))
        }
        (SubMsgResult::Err(err), REPLY_ID_LIQUIDATOR) => Err(StdError::generic_err(err).into()),
        (SubMsgResult::Ok(_), _) => Ok(Response::default()),
        _ => Err(ContractError::Unauthorized {}),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn sudo(deps: DepsMut, _env: Env, msg: SudoMsg) -> Result<Response, ContractError> {
    let mut config = Config::load(deps.storage)?;

    match msg {
        SudoMsg::SetVault { address } => {
            let vault: Vault = (&deps.api.addr_validate(&address)?).into();
            let denom = vault.config(deps.querier)?.denom;

            if !config.collateral_ratios.contains_key(&denom) {
                return Err(ContractError::InvalidCollateral { denom });
            }
            BORROW.save(deps.storage, denom, &vault)?;
            Ok(Response::default())
        }
        SudoMsg::SetCollateral {
            denom,
            collateralization_ratio,
        } => {
            config
                .collateral_ratios
                .insert(denom, collateralization_ratio);
            config.validate()?;
            config.save(deps.storage)?;
            Ok(Response::default())
        }
        SudoMsg::UpdateConfig(update) => {
            config.update(&update);
            config.validate()?;
            config.save(deps.storage)?;
            Ok(Response::default())
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> Result<Binary, ContractError> {
    let config = Config::load(deps.storage)?;

    match msg {
        QueryMsg::Config {} => Ok(to_json_binary(&ConfigResponse::from(config))?),
        QueryMsg::Borrows {} => Ok(to_json_binary(&ghost::vault::BorrowersResponse {
            borrowers: config
                .collateral_ratios
                .keys()
                .map(|x| {
                    Ok(BORROW
                        .load(deps.storage, x.clone())?
                        .borrower(deps.querier, &env.contract.address)?)
                })
                .collect::<Result<Vec<ghost::vault::BorrowerResponse>, ContractError>>()?,
        })?),
        QueryMsg::Account(addr) => Ok(to_json_binary(&AccountResponse::from(
            CreditAccount::load(
                deps,
                &config,
                &env.contract.address,
                deps.api.addr_validate(&addr)?,
            )?,
        ))?),

        QueryMsg::Accounts { owner, tag } => Ok(to_json_binary(&AccountsResponse {
            accounts: CreditAccount::by_owner(
                deps,
                &config,
                env.contract.address,
                &deps.api.addr_validate(&owner)?,
                tag,
            )?
            .iter()
            .map(|x| AccountResponse::from(x.clone()))
            .collect(),
        })?),

        QueryMsg::AllAccounts { cursor, limit } => Ok(to_json_binary(&AccountsResponse {
            accounts: CreditAccount::list(
                deps,
                &config,
                &env.contract.address,
                cursor.map(|x| deps.api.addr_validate(&x)).transpose()?,
                limit,
            )?
            .iter()
            .map(|x| AccountResponse::from(x.clone()))
            .collect(),
        })?),

        QueryMsg::Predict { owner, salt } => {
            let a = &CreditAccount::create(
                deps,
                config.code_id,
                env.contract.address,
                deps.api.addr_validate(&owner)?,
                "".to_string(),
                "".to_string(),
                salt,
            )?
            .0;
            Ok(to_json_binary(&a.id())?)
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: ()) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::default())
}
