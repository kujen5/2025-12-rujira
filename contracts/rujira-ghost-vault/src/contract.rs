use crate::borrowers::Borrower;
use crate::config::Config;
use crate::error::ContractError;
use crate::events::{event_borrow, event_deposit, event_repay, event_withdraw};
use crate::state::State;
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    coins, to_json_binary, BankMsg, Binary, Deps, DepsMut, Empty, Env, MessageInfo, Response,
    StdResult, Uint128,
};
use cw2::set_contract_version;
use cw_utils::must_pay;
use rujira_rs::ghost::vault::{
    BorrowerResponse, BorrowersResponse, ConfigResponse, DelegateResponse, ExecuteMsg,
    InstantiateMsg, MarketMsg, PoolResponse, QueryMsg, StatusResponse, SudoMsg,
};
use rujira_rs::TokenFactory;
use std::cmp::min;

const CONTRACT_NAME: &str = env!("CARGO_PKG_NAME");
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// ## OK `1st`
/// ### constructor
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?; //e migration tracking per version
                                                                          //e validate, save and persist config
    let config = Config::new(deps.api, msg.clone())?;
    config.validate()?;
    config.save(deps.storage)?;
    //e initialize the deposit and debt pools
    State::init(deps.storage, &env)?;
    //e create the namespace for receipt tokens (upon minting) (e.g. x/ghost-vault/btc)
    let rcpt = TokenFactory::new(&env, format!("ghost-vault/{}", config.denom).as_str());

    Ok(Response::default().add_message(rcpt.create_msg(msg.receipt)))
}

//e user facing methods
#[cfg_attr(not(feature = "library"), entry_point)]
/// (owner.clone(),contract.clone(),&ExecuteMsg::Deposit { callback: None },&coins(1_000u128, "btc"),)
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    //e update the state with accrued interest - invariant
    let config = Config::load(deps.storage)?;
    let mut state = State::load(deps.storage)?;
    let rcpt = TokenFactory::new(&env, format!("ghost-vault/{}", config.denom).as_str());+
    //e accrue interest based on time -> increase deposit_pool.size() by interest -> increase debt_pool.size() by interest+fee -> last updated = now
    let fees = state.distribute_interest(&env, &config)?;
    //e depends on the msg you send (what you wanna do)
    let mut response = match msg {
        ExecuteMsg::Deposit { callback } => {
            let amount = must_pay(&info, config.denom.as_str())?;
            let mint = state.deposit(amount)?;
            state.save(deps.storage)?;

            match callback {
                None => Response::default()
                    .add_message(rcpt.mint_msg(mint, info.sender.clone()))
                    .add_event(event_deposit(info.sender, amount, mint)),
                Some(cb) => Response::default()
                    .add_message(rcpt.mint_msg(mint, env.contract.address))
                    .add_message(cb.to_message(
                        &info.sender,
                        Empty {},
                        coins(mint.u128(), rcpt.denom()),
                    )?)
                    .add_event(event_deposit(info.sender, amount, mint)),
            }
        }
        ExecuteMsg::Withdraw { callback } => {
            let amount = must_pay(&info, rcpt.denom().as_str())?;
            let withdrawn = state.withdraw(amount)?;
            state.save(deps.storage)?;

            match callback {
                None => Response::default()
                    .add_message(rcpt.burn_msg(amount))
                    .add_message(BankMsg::Send {
                        to_address: info.sender.to_string(),
                        amount: coins(withdrawn.u128(), config.denom),
                    })
                    .add_event(event_withdraw(info.sender, withdrawn, amount)),
                Some(cb) => Response::default()
                    .add_message(rcpt.burn_msg(amount))
                    .add_message(cb.to_message(
                        &info.sender,
                        Empty {},
                        coins(withdrawn.u128(), &config.denom),
                    )?)
                    .add_event(event_withdraw(info.sender, withdrawn, amount)),
            }
        }
        ExecuteMsg::Market(market_msg) => {
            let mut borrower = Borrower::load(deps.storage, info.sender.clone())?;
            execute_market(deps, info, &mut state, market_msg, &mut borrower)?
        }
    };
    if fees.gt(&Uint128::zero()) {
        response = response.add_message(rcpt.mint_msg(fees, config.fee_address.clone()));
    }

    Ok(response)
}

pub fn execute_market(
    deps: DepsMut,
    info: MessageInfo,
    state: &mut State,
    msg: MarketMsg,
    borrower: &mut Borrower,
) -> Result<Response, ContractError> {
    let config = Config::load(deps.storage)?;
    let response = match msg {
        MarketMsg::Borrow {
            amount,
            callback,
            delegate,
        } => {
            let shares = state.borrow(amount)?;
            match delegate.clone() {
                Some(d) => {
                    borrower.delegate_borrow(
                        deps.storage,
                        deps.api.addr_validate(&d)?,
                        &state.debt_pool,
                        shares,
                    )?;
                }
                None => {
                    borrower.borrow(deps.storage, &state.debt_pool, shares)?;
                }
            };

            match callback {
                None => Response::default()
                    .add_message(BankMsg::Send {
                        to_address: info.sender.to_string(),
                        amount: coins(amount.u128(), config.denom),
                    })
                    .add_event(event_borrow(
                        borrower.addr.clone(),
                        delegate,
                        amount,
                        shares,
                    )),
                Some(cb) => Response::default()
                    .add_message(cb.to_message(
                        &info.sender,
                        Empty {},
                        coins(amount.u128(), &config.denom),
                    )?)
                    .add_event(event_borrow(
                        borrower.addr.clone(),
                        delegate,
                        amount,
                        shares,
                    )),
            }
        }
        MarketMsg::Repay { delegate } => {
            let amount = must_pay(&info, config.denom.as_str())?;
            let delegate_address = delegate
                .clone()
                .map(|d| deps.api.addr_validate(&d))
                .transpose()?;

            let borrower_shares = match delegate_address.as_ref() {
                Some(d) => borrower.delegate_shares(deps.storage, d.clone()),
                None => borrower.shares,
            };
            let borrower_debt = state.debt_pool.ownership(borrower_shares);
            let repay_amount = min(amount, borrower_debt);

            let shares = state.repay(repay_amount)?;

            match delegate_address.clone() {
                Some(d) => borrower.delegate_repay(deps.storage, d, shares),
                None => borrower.repay(deps.storage, shares),
            }?;

            let mut response = Response::default().add_event(event_repay(
                borrower.addr.clone(),
                delegate,
                repay_amount,
                shares,
            ));

            let refund = amount.checked_sub(repay_amount)?;
            if !refund.is_zero() {
                response = response.add_message(BankMsg::Send {
                    to_address: info.sender.to_string(),
                    amount: coins(refund.u128(), &config.denom),
                });
            }
            response
        }
    };
    state.save(deps.storage)?;
    Ok(response)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn sudo(deps: DepsMut, _env: Env, msg: SudoMsg) -> Result<Response, ContractError> {
    let mut config = Config::load(deps.storage)?;

    match msg {
        SudoMsg::SetBorrower { contract, limit } => {
            Borrower::set(deps.storage, deps.api.addr_validate(&contract)?, limit)?;
            Ok(Response::default())
        }
        SudoMsg::SetInterest(interest) => {
            interest.validate()?;
            config.interest = interest;
            config.save(deps.storage)?;
            Ok(Response::default())
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> Result<Binary, ContractError> {
    let mut state = State::load(deps.storage)?;
    let config = Config::load(deps.storage)?;
    state.distribute_interest(&env, &config)?;

    match msg {
        QueryMsg::Config {} => Ok(to_json_binary(&ConfigResponse {
            denom: config.denom,
            interest: config.interest,
        })?),

        QueryMsg::Status {} => Ok(to_json_binary(&StatusResponse {
            debt_rate: state.debt_rate(&config.interest)?,
            lend_rate: state.lend_rate(&config.interest)?,
            utilization_ratio: state.utilization(),
            last_updated: state.last_updated,
            debt_pool: PoolResponse {
                size: state.debt_pool.size(),
                shares: state.debt_pool.shares(),
                ratio: state.debt_pool.ratio(),
            },
            deposit_pool: PoolResponse {
                size: state.deposit_pool.size(),
                shares: state.deposit_pool.shares(),
                ratio: state.deposit_pool.ratio(),
            },
        })?),
        QueryMsg::Borrower { addr } => {
            let borrower = Borrower::load(deps.storage, deps.api.addr_validate(&addr)?)?;
            let current = state.debt_pool.ownership(borrower.shares);
            Ok(to_json_binary(&BorrowerResponse {
                addr: borrower.addr.to_string(),
                denom: config.denom,
                limit: borrower.limit,
                current,
                shares: borrower.shares,
                available: min(
                    // Current borrows can exceed limit due to interest
                    borrower.limit.checked_sub(current).unwrap_or_default(),
                    state.deposit_pool.size() - state.debt_pool.size(),
                ),
            })?)
        }
        QueryMsg::Delegate { borrower, addr } => {
            let borrower = Borrower::load(deps.storage, deps.api.addr_validate(&borrower)?)?;
            let delegate = borrower.delegate_shares(deps.storage, deps.api.addr_validate(&addr)?);
            let current = state.debt_pool.ownership(borrower.shares);

            Ok(to_json_binary(&DelegateResponse {
                borrower: BorrowerResponse {
                    addr: borrower.addr.to_string(),
                    denom: config.denom,
                    limit: borrower.limit,
                    current,
                    shares: borrower.shares,
                    available: min(
                        borrower.limit.checked_sub(current).unwrap_or_default(),
                        state.deposit_pool.size() - state.debt_pool.size(),
                    ),
                },
                addr,
                current: state.debt_pool.ownership(delegate),
                shares: delegate,
            })?)
        }
        QueryMsg::Borrowers { limit, start_after } => {
            let borrowers = Borrower::list(
                deps.storage,
                limit,
                start_after
                    .map(|x| deps.api.addr_validate(x.as_str()))
                    .transpose()?,
            )
            .map(|x| {
                x.map(|borrower| {
                    let current = state.debt_pool.ownership(borrower.shares);

                    BorrowerResponse {
                        addr: borrower.addr.to_string(),
                        denom: config.denom.clone(),
                        limit: borrower.limit,
                        current,
                        shares: borrower.shares,
                        available: min(
                            // Current borrows can exceed limit due to interest
                            borrower.limit.checked_sub(current).unwrap_or_default(),
                            state.deposit_pool.size() - state.debt_pool.size(),
                        ),
                    }
                })
            })
            .collect::<StdResult<Vec<BorrowerResponse>>>()?;
            Ok(to_json_binary(&BorrowersResponse { borrowers })?)
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: ()) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    crate::borrowers::migrate(deps.storage)?;
    Ok(Response::default())
}

#[cfg(all(test, feature = "mock"))]
mod tests {

    use std::str::FromStr;

    use super::*;
    use cosmwasm_std::{coin, Decimal, Event, Uint128};
    use cw_multi_test::{ContractWrapper, Executor};
    use rujira_rs::{ghost::vault::Interest, TokenMetadata};
    use rujira_rs_testing::mock_rujira_app;

    #[test]
    fn lifecycle() {
        let mut app = mock_rujira_app();
        let owner = app.api().addr_make("owner");
        let borrower = app.api().addr_make("borrower");

        app.init_modules(|router, _, storage| {
            router
                .bank
                .init_balance(storage, &owner, coins(1_000_000, "btc"))
                .unwrap();
            router
                .bank
                .init_balance(storage, &borrower, coins(1_000_000, "btc"))
                .unwrap();
        });

        let code = Box::new(ContractWrapper::new(execute, instantiate, query).with_sudo(sudo));
        let code_id = app.store_code(code);
        let contract = app
            .instantiate_contract(
                code_id,
                owner.clone(),
                &InstantiateMsg {
                    denom: "btc".to_string(),
                    receipt: TokenMetadata {
                        description: "".to_string(),
                        display: "".to_string(),
                        name: "".to_string(),
                        symbol: "".to_string(),
                        uri: None,
                        uri_hash: None,
                    },
                    interest: Interest {
                        target_utilization: Decimal::from_ratio(8u128, 10u128),
                        base_rate: Decimal::from_ratio(1u128, 10u128),
                        step1: Decimal::from_ratio(1u128, 10u128),
                        step2: Decimal::from_ratio(3u128, 1u128),
                    },
                    fee: Decimal::zero(),
                    fee_address: owner.to_string(),
                },
                &[],
                "template",
                None,
            )
            .unwrap();

        // First deposit
        let res = app
            .execute_contract(
                owner.clone(),
                contract.clone(),
                &ExecuteMsg::Deposit { callback: None },
                &coins(1_000u128, "btc"),
            )
            .unwrap();

        res.assert_event(
            &Event::new("wasm-rujira-ghost-vault/deposit").add_attributes(vec![
                ("amount", "1000"),
                ("owner", owner.as_str()),
                ("shares", "1000"),
            ]),
        );

        res.assert_event(&Event::new("mint").add_attributes(vec![
            ("amount", "1000"),
            ("denom", "x/ghost-vault/btc"),
            ("recipient", owner.as_str()),
        ]));

        // Withdraw some

        let res = app
            .execute_contract(
                owner.clone(),
                contract.clone(),
                &ExecuteMsg::Withdraw { callback: None },
                &coins(200u128, "x/ghost-vault/btc"),
            )
            .unwrap();

        res.assert_event(
            &Event::new("wasm-rujira-ghost-vault/withdraw").add_attributes(vec![
                ("amount", "200"),
                ("owner", owner.as_str()),
                ("shares", "200"),
            ]),
        );

        res.assert_event(
            &Event::new("burn")
                .add_attributes(vec![("amount", "200"), ("denom", "x/ghost-vault/btc")]),
        );

        // Whitelist a borrower address
        app.wasm_sudo(
            contract.clone(),
            &SudoMsg::SetBorrower {
                contract: borrower.to_string(),
                limit: Uint128::from(500u128),
            },
        )
        .unwrap();

        let b: BorrowerResponse = app
            .wrap()
            .query_wasm_smart(
                contract.clone(),
                &QueryMsg::Borrower {
                    addr: borrower.to_string(),
                },
            )
            .unwrap();
        assert_eq!(b.addr, borrower.to_string());
        assert_eq!(b.limit, Uint128::from(500u128));
        assert_eq!(b.current, Uint128::zero());

        // Check we can't borrow more than the limit
        app.execute_contract(
            borrower.clone(),
            contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(501u128),
                delegate: None,
            }),
            &[],
        )
        .unwrap_err();

        // Borrow the whole lot,
        let res = app
            .execute_contract(
                borrower.clone(),
                contract.clone(),
                &ExecuteMsg::Market(MarketMsg::Borrow {
                    callback: None,
                    amount: Uint128::from(500u128),
                    delegate: None,
                }),
                &[],
            )
            .unwrap();

        res.assert_event(
            &Event::new("wasm-rujira-ghost-vault/borrow").add_attributes(vec![
                ("borrower", borrower.as_str()),
                ("amount", "500"),
                ("shares", "500"),
            ]),
        );

        res.assert_event(
            &Event::new("transfer")
                .add_attributes(vec![("amount", "500btc"), ("recipient", borrower.as_str())]),
        );

        // Now repay with the required asset
        let res = app
            .execute_contract(
                borrower.clone(),
                contract.clone(),
                &ExecuteMsg::Market(MarketMsg::Repay { delegate: None }),
                &[coin(100, "btc")],
            )
            .unwrap();

        res.assert_event(
            &Event::new("wasm-rujira-ghost-vault/repay").add_attributes(vec![
                ("amount", "100"),
                ("borrower", borrower.as_str()),
                ("shares", "100"),
            ]),
        );

        app.update_block(|x| x.time = x.time.plus_days(90));

        // Check the rate has increased
        let status: StatusResponse = app
            .wrap()
            .query_wasm_smart(contract.clone(), &QueryMsg::Status {})
            .unwrap();
        assert_eq!(
            status.utilization_ratio,
            Decimal::from_str("0.509803921568627451").unwrap()
        );
        assert_eq!(status.debt_pool.size, Uint128::from(416u128));
        assert_eq!(status.debt_pool.shares, Uint128::from(400u128));
        assert_eq!(status.debt_pool.ratio, Decimal::from_str("1.04").unwrap());
        assert_eq!(status.deposit_pool.size, Uint128::from(816u128));
        assert_eq!(status.deposit_pool.shares, Uint128::from(800u128));
        assert_eq!(
            status.deposit_pool.ratio,
            Decimal::from_str("1.02").unwrap()
        );

        // Make another deposit
        let res = app
            .execute_contract(
                owner.clone(),
                contract.clone(),
                &ExecuteMsg::Deposit { callback: None },
                &coins(1_000u128, "btc"),
            )
            .unwrap();

        // Ensure that < 1000 tokens are minted to accommodate the increase in interest payments
        res.assert_event(
            &Event::new("wasm-rujira-ghost-vault/deposit").add_attributes(vec![
                ("amount", "1000"),
                ("owner", owner.as_str()),
                ("shares", "980"),
            ]),
        );

        res.assert_event(&Event::new("mint").add_attributes(vec![
            ("amount", "980"),
            ("denom", "x/ghost-vault/btc"),
            ("recipient", owner.as_str()),
        ]));

        // finally check that a 1:1 repay doesn't work, and that more btc is required

        // debt rate is 1.0325

        let res = app
            .execute_contract(
                borrower.clone(),
                contract.clone(),
                &ExecuteMsg::Market(MarketMsg::Repay { delegate: None }),
                &[coin(104, "btc")],
            )
            .unwrap();
        res.assert_event(
            &Event::new("wasm-rujira-ghost-vault/repay").add_attributes(vec![
                ("amount", "104"),
                ("borrower", borrower.as_str()),
                ("shares", "100"),
            ]),
        );

        // Lastly check that the value of my deposit has increased
        let res = app
            .execute_contract(
                owner.clone(),
                contract.clone(),
                &ExecuteMsg::Withdraw { callback: None },
                &coins(200u128, "x/ghost-vault/btc"),
            )
            .unwrap();

        res.assert_event(
            &Event::new("wasm-rujira-ghost-vault/withdraw").add_attributes(vec![
                ("amount", "204"),
                ("owner", owner.as_str()),
                ("shares", "200"),
            ]),
        );

        res.assert_event(
            &Event::new("burn")
                .add_attributes(vec![("amount", "200"), ("denom", "x/ghost-vault/btc")]),
        );

        // Check complete repayument
        app.execute_contract(
            borrower.clone(),
            contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Repay { delegate: None }),
            &[coin(312, "btc")],
        )
        .unwrap();

        // Check the rate has increased
        let status: StatusResponse = app
            .wrap()
            .query_wasm_smart(contract.clone(), &QueryMsg::Status {})
            .unwrap();

        assert_eq!(status.utilization_ratio, Decimal::zero());
        assert_eq!(status.debt_pool.size, Uint128::zero());
        assert_eq!(status.debt_pool.shares, Uint128::zero());
        assert_eq!(status.debt_pool.ratio, Decimal::zero());
        assert_eq!(status.deposit_pool.size, Uint128::from(1612u128));
        assert_eq!(status.deposit_pool.shares, Uint128::from(1580u128));
        assert_eq!(
            status.deposit_pool.ratio,
            Decimal::from_str("1.020253164556962025").unwrap()
        );
    }
}
