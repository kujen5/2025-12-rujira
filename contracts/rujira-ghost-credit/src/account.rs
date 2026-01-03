use crate::{config::Config, state::BORROW, ContractError};
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{ensure, Addr, Binary, Decimal, Deps, DepsMut, Order, StdResult, WasmMsg};
use cw_storage_plus::{Bound, Index, IndexList, IndexedMap, MultiIndex};
use cw_utils::NativeBalance;
use rujira_rs::{
    account::Account,
    ghost::credit::{
        AccountResponse, Collateral, CollateralResponse, Debt, DebtResponse, LiquidateMsg,
        LiquidationPreferences,
    },
    NativeBalancePlus, OracleValue,
};
use sha2::{Digest, Sha256};
use std::ops::{Add, Div};
pub static ACCOUNTS_KEY: &str = "a";
pub static ACCOUNTS_KEY_OWNER: &str = "a__o";
pub static ACCOUNTS_KEY_OWNER_TAG: &str = "a__ot";
pub static ACCOUNTS_KEY_TAG: &str = "a__t";

#[cw_serde]
struct Stored {
    owner: Addr,
    account: Addr,
    #[serde(default)]
    tag: String,
    liquidation_preferences: LiquidationPreferences,
}

#[cw_serde]
pub struct CreditAccount {
    pub owner: Addr,
    pub tag: String,
    pub account: Account,
    pub collaterals: Vec<Valued<Collateral>>,
    pub debts: Vec<Valued<Debt>>,
    pub liquidation_preferences: LiquidationPreferences,
}

#[cw_serde]
pub struct Valued<T> {
    pub value: Decimal,
    pub value_adjusted: Decimal,
    pub item: T,
}

pub struct AccountIndexes<'a> {
    owner: MultiIndex<'a, Addr, Stored, String>,
    owner_tag: MultiIndex<'a, (Addr, String), Stored, String>,
    tag: MultiIndex<'a, String, Stored, String>,
}

impl<'a> IndexList<Stored> for AccountIndexes<'a> {
    fn get_indexes(&'_ self) -> Box<dyn Iterator<Item = &'_ dyn Index<Stored>> + '_> {
        let v: Vec<&dyn Index<Stored>> = vec![&self.owner, &self.owner_tag, &self.tag];
        Box::new(v.into_iter())
    }
}

impl CreditAccount {
    pub fn id(&self) -> Addr {
        self.account.contract()
    }
    pub fn new(owner: Addr, account: Account, tag: String) -> Self {
        Self {
            owner,
            account,
            tag,
            collaterals: Default::default(),
            debts: Default::default(),
            liquidation_preferences: Default::default(),
        }
    }

    ///Ok1st
    pub fn create(
        deps: Deps,
        code_id: u64,
        admin: Addr,
        owner: Addr,
        label: String,
        tag: String,
        salt: Binary,
    ) -> Result<(Self, WasmMsg), ContractError> {
        let mut hasher = Sha256::new();
        //e add owner address to hash
        //e same salt + different owner → different account
        hasher.update(owner.as_bytes());
        //e add user supplied salt
        hasher.update(salt.as_slice());
        //e binary -> vec<u8>
        let mut salt = salt.to_vec();
        salt.append(&mut deps.api.addr_canonicalize(owner.as_ref())?.to_vec());
        //e instatiate the account
        let (account, msg) = Account::create(
            deps,
            admin,
            code_id,
            format!("ghost-credit/{label}"),
            Binary::from(hasher.finalize().to_vec()),
        )?;
        let acc = Self::new(owner, account, tag);
        Ok((acc, msg))
    }
    pub fn save(&self, deps: DepsMut) -> StdResult<()> {
        Self::store().save(deps.storage, self.account.contract(), &Stored::from(self))
    }

    pub fn by_owner(
        deps: Deps,
        config: &Config,
        contract: Addr,
        owner: &Addr,
        tag: Option<String>,
    ) -> Result<Vec<Self>, ContractError> {
        match tag {
            Some(tag) => Self::store().idx.owner_tag.prefix((owner.clone(), tag)),
            None => Self::store().idx.owner.prefix(owner.clone()),
        }
        .range(deps.storage, None, None, Order::Descending)
        .map::<Result<Self, ContractError>, _>(|x| match x {
            Ok((_, stored)) => stored.to_credit_account(deps, &contract, config),
            Err(err) => Err(ContractError::Std(err)),
        })
        .collect()
    }

    pub fn list(
        deps: Deps,
        config: &Config,
        contract: &Addr,
        cursor: Option<Addr>,
        limit: Option<usize>,
    ) -> Result<Vec<Self>, ContractError> {
        Self::store()
            .range(
                deps.storage,
                cursor.map(Bound::exclusive),
                None,
                Order::Ascending,
            )
            .take(limit.unwrap_or(100))
            .map(|res| res?.1.to_credit_account(deps, contract, config))
            .collect()
    }

    pub fn load(
        deps: Deps,
        config: &Config,
        contract: &Addr,
        account: Addr,
    ) -> Result<Self, ContractError> {
        Self::store()
            .load(deps.storage, account)?
            .to_credit_account(deps, contract, config)
    }

    pub fn adjusted_ltv(&self) -> Decimal {
        let collateral = self
            .collaterals
            .iter()
            .map(|x| x.value_adjusted)
            .collect::<Vec<Decimal>>()
            .into_iter()
            .reduce(|a, b| a + b)
            .unwrap_or_default();

        let debt = self
            .debts
            .iter()
            .map(|x| x.value)
            .collect::<Vec<Decimal>>()
            .into_iter()
            .reduce(|a, b| a + b)
            .unwrap_or_default();

        if debt.is_zero() {
            return Decimal::zero();
        }

        debt.div(collateral)
    }

    pub fn check_safe(&self, limit: &Decimal) -> Result<(), ContractError> {
        ensure!(
            self.adjusted_ltv().lt(limit),
            ContractError::Unsafe {
                ltv: self.adjusted_ltv()
            }
        );
        Ok(())
    }

    pub fn check_unsafe(&self, limit: &Decimal) -> Result<(), ContractError> {
        ensure!(self.adjusted_ltv().ge(limit), ContractError::Safe {});
        Ok(())
    }

    fn store<'a>() -> IndexedMap<Addr, Stored, AccountIndexes<'a>> {
        IndexedMap::new(
            ACCOUNTS_KEY,
            AccountIndexes {
                owner: MultiIndex::new(
                    |_k, d: &Stored| d.owner.clone(),
                    ACCOUNTS_KEY,
                    ACCOUNTS_KEY_OWNER,
                ),
                owner_tag: MultiIndex::new(
                    |_k, d: &Stored| (d.owner.clone(), d.tag.clone()),
                    ACCOUNTS_KEY,
                    ACCOUNTS_KEY_OWNER_TAG,
                ),
                tag: MultiIndex::new(
                    |_k, d: &Stored| d.tag.clone(),
                    ACCOUNTS_KEY,
                    ACCOUNTS_KEY_TAG,
                ),
            },
        )
    }

    pub fn set_preference_order(
        &mut self,
        denom: &String,
        after: &Option<String>,
    ) -> Result<(), ContractError> {
        match after {
            Some(x) => self
                .liquidation_preferences
                .order
                .insert(denom.clone(), x.clone())?,
            None => self.liquidation_preferences.order.remove(denom),
        };
        Ok(())
    }

    pub fn set_preference_msgs(&mut self, msgs: Vec<LiquidateMsg>) {
        self.liquidation_preferences.messages = msgs
    }

    fn balance(&self) -> NativeBalance {
        self.collaterals
            .iter()
            .fold(NativeBalance::default(), |agg, v| v.item.balance().add(agg))
    }

    fn debt(&self) -> NativeBalance {
        self.debts.iter().fold(NativeBalance::default(), |agg, v| {
            NativeBalance::from(&v.item).add(agg)
        })
    }

    /// Validates the slippage of the liquidation against config limits, and liquidation order preference has been respected
    pub fn validate_liquidation(
        &self,
        deps: Deps,
        config: &Config,
        old: &Self,
    ) -> Result<(), ContractError> {
        let balance = self.balance();
        let spent = old.balance().sent(&balance);

        for coin in spent.clone().into_vec() {
            self.liquidation_preferences
                .order
                .validate(&coin, &balance)?;
        }

        let spent_usd = spent.value_usd(deps.querier)?;
        let repaid = old.debt().sent(&self.debt());
        let repaid_usd = repaid.value_usd(deps.querier)?;
        let slippage = spent_usd
            .checked_sub(repaid_usd)
            .unwrap_or_default()
            .checked_div(spent_usd)
            .unwrap_or_default();

        // Check against config liquidation slip
        if !slippage.is_zero() {
            ensure!(
                slippage.le(&config.liquidation_max_slip),
                ContractError::LiquidationMaxSlipExceeded { slip: slippage }
            );
        }

        Ok(())
    }
}

impl Stored {
    fn to_credit_account(
        &self,
        deps: Deps,
        contract: &Addr,
        config: &Config,
    ) -> Result<CreditAccount, ContractError> {
        let mut ca = CreditAccount {
            owner: self.owner.clone(),
            tag: self.tag.clone(),
            account: Account::load(deps, &self.account)?,
            collaterals: vec![],
            debts: vec![],
            liquidation_preferences: self.liquidation_preferences.clone(),
        };

        for denom in config.collateral_ratios.keys() {
            let item = Collateral::try_from(&deps.querier.query_balance(&self.account, denom)?)?;
            if item.value_usd(deps.querier)?.is_zero() {
                continue;
            }
            ca.collaterals.push(Valued {
                value: item.value_usd(deps.querier)?,
                value_adjusted: item.value_adjusted(deps, &config.collateral_ratios)?,
                item,
            });
        }

        for vault in BORROW.range(deps.storage, None, None, Order::Ascending) {
            let debt = Debt::from(vault?.1.delegate(deps.querier, contract, &self.account)?);
            let value = debt.value_usd(deps.querier)?;
            if value.is_zero() {
                continue;
            }
            ca.debts.push(Valued {
                item: debt,
                value,
                value_adjusted: value,
            });
        }

        Ok(ca)
    }
}

impl From<&CreditAccount> for Stored {
    fn from(value: &CreditAccount) -> Self {
        Self {
            owner: value.owner.clone(),
            tag: value.tag.clone(),
            account: value.account.contract(),
            liquidation_preferences: value.liquidation_preferences.clone(),
        }
    }
}

impl From<CreditAccount> for AccountResponse {
    fn from(value: CreditAccount) -> Self {
        Self {
            ltv: value.adjusted_ltv(),
            tag: value.tag,
            owner: value.owner,
            account: value.account.contract(),
            collaterals: value
                .collaterals
                .iter()
                .map(CollateralResponse::from)
                .collect(),
            debts: value.debts.iter().map(DebtResponse::from).collect(),
            liquidation_preferences: value.liquidation_preferences,
        }
    }
}

impl From<&Valued<Debt>> for DebtResponse {
    fn from(value: &Valued<Debt>) -> Self {
        Self {
            debt: value.item.clone(),
            value: value.value,
        }
    }
}

impl From<&Valued<Collateral>> for CollateralResponse {
    fn from(value: &Valued<Collateral>) -> Self {
        Self {
            collateral: value.item.clone(),
            value_full: value.value,
            value_adjusted: value.value_adjusted,
        }
    }
}
