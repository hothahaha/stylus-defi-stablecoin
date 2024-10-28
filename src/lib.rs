#![cfg_attr(not(feature = "export-abi"), no_main)]
extern crate alloc;

mod decentralized_stable_coin;
mod erc20;

use alloy_sol_types::sol;
use decentralized_stable_coin::{DecentralizedStableCoin, DecentralizedStableCoinError};
use stylus_sdk::{
    alloy_primitives::{Address, U256},
    call::Call,
    call::MethodError,
    evm, msg,
    prelude::*,
};

sol! {
    event CollateralDeposited(address indexed user, address indexed token, uint256 amount);
    event CollateralRedeemed(
        address indexed redeemedFrom, address indexed redeemedTo, uint256 indexed amount, address token
    );

    error TokenAddressesAndPriceFeedAddressesMustBeSameLength();
    error NeedsMoreThanZero();
    error NotAllowedToken();
    error TransferFailed();
    error BreaksHealthFactor(uint256);
    error MintFailed();
    error HealthFactorOk();
    error HealthFactorNotImproved();
    error PriceFeedError();
    error ConversionError();
}

// Assuming we have these imports available
#[derive(SolidityError)]
pub enum DSCEngineError {
    NeedsMoreThanZero(NeedsMoreThanZero),
    TokenAddressesAndPriceFeedAddressesMustBeSameLength(
        TokenAddressesAndPriceFeedAddressesMustBeSameLength,
    ),
    NotAllowedToken(NotAllowedToken),
    TransferFailed(TransferFailed),
    BreaksHealthFactor(BreaksHealthFactor),
    MintFailed(MintFailed),
    HealthFactorOk(HealthFactorOk),
    HealthFactorNotImproved(HealthFactorNotImproved),
    PriceFeedError(PriceFeedError),
    ConversionError(ConversionError),
    DecentralizedStableCoinError(DecentralizedStableCoinError),
}

sol_interface! {
    interface IAggregatorV3 {
        function latestRoundData()
    external
    view
    returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
    }
    interface IERC20 {
        function transfer(address to, uint256 value) external returns (bool);
        function transferFrom(address from, address to, uint256 value) external returns (bool);
    }
}

sol_storage! {
    #[entrypoint]
    pub struct DSCEngine {
        uint256 additional_feed_precision;
        uint256 precision;
        uint256 liquidation_threshold;
        uint256 liquidation_precision;
        uint256 min_health_factor;
        uint256 liquidation_bonus;
        mapping(address => address) price_feeds;
        mapping(address => mapping(address => uint256)) collateral_deposited;
        mapping(address => uint256) dsc_minted;
        address[] collateral_tokens;
        DecentralizedStableCoin dsc;
    }
}

#[public]
impl DSCEngine {
    pub fn constructor(
        &mut self,
        token_addresses: Vec<Address>,
        price_feed_addresses: Vec<Address>,
    ) -> Result<(), DSCEngineError> {
        if token_addresses.len() != price_feed_addresses.len() {
            return Err(
                DSCEngineError::TokenAddressesAndPriceFeedAddressesMustBeSameLength(
                    TokenAddressesAndPriceFeedAddressesMustBeSameLength {},
                ),
            );
        }
        for (token, price_feed) in token_addresses.iter().zip(price_feed_addresses.iter()) {
            self.price_feeds.insert(*token, *price_feed);
            self.collateral_tokens.push(*token);
        }

        // 修改这部分
        let mut dsc = DecentralizedStableCoin::default();
        dsc.constructor();
        self.dsc = dsc;

        self.additional_feed_precision
            .set(U256::from(10).pow(U256::from(10)));
        self.precision.set(U256::from(10).pow(U256::from(18)));
        self.liquidation_threshold.set(U256::from(50));
        self.liquidation_precision.set(U256::from(100));
        self.min_health_factor
            .set(U256::from(10).pow(U256::from(18)));
        self.liquidation_bonus.set(U256::from(10));
        Ok(())
    }

    pub fn deposit_collateral_and_mint_dsc(
        &mut self,
        token_collateral_address: Address,
        amount_collateral: U256,
        amount_dsc_to_mint: U256,
    ) -> Result<(), DSCEngineError> {
        let _ = self.deposit_collateral(token_collateral_address, amount_collateral);
        self.mint_dsc(amount_dsc_to_mint)?;
        Ok(())
    }

    pub fn deposit_collateral(
        &mut self,
        token_collateral_address: Address,
        amount_collateral: U256,
    ) -> Result<(), DSCEngineError> {
        if amount_collateral == U256::ZERO {
            return Err(DSCEngineError::NeedsMoreThanZero(NeedsMoreThanZero {}));
        }
        if self.price_feeds.get(token_collateral_address).is_zero() {
            return Err(DSCEngineError::NotAllowedToken(NotAllowedToken {}));
        }

        let sender = msg::sender();
        let user_collateral_mapping = self.collateral_deposited.getter(sender);
        let user_collateral = user_collateral_mapping.getter(token_collateral_address);
        let value = user_collateral.get();
        self.collateral_deposited
            .setter(sender)
            .setter(token_collateral_address)
            .set(value + amount_collateral);

        evm::log(CollateralDeposited {
            user: sender,
            token: token_collateral_address,
            amount: amount_collateral,
        });

        let token = IERC20::new(token_collateral_address);
        if token
            .transfer_from(Call::new(), sender, Address::ZERO, amount_collateral)
            .is_err()
        {
            return Err(DSCEngineError::TransferFailed(TransferFailed {}));
        }
        Ok(())
    }

    fn _burn_dsc(&mut self, amount_dsc_to_burn: U256, on_behalf_of: Address, dsc_from: Address) {
        let user_dsc_minted = self.dsc_minted.getter(on_behalf_of);
        let value = user_dsc_minted.get();
        self.dsc_minted
            .setter(on_behalf_of)
            .set(value - amount_dsc_to_burn);

        if !self
            .dsc
            .transfer_from(dsc_from, Address::ZERO, amount_dsc_to_burn)
            .is_err()
        {
            panic!("TransferFailed");
        }
        let _ = self.dsc.burn(amount_dsc_to_burn);
    }

    fn _redeem_collateral(
        &mut self,
        token_collateral_address: Address,
        amount_collateral: U256,
        from: Address,
        to: Address,
    ) -> Result<(), DSCEngineError> {
        let user_collateral_mapping = self.collateral_deposited.getter(from);
        let user_collateral = user_collateral_mapping.getter(token_collateral_address);
        let value = user_collateral.get();
        self.collateral_deposited
            .setter(from)
            .setter(token_collateral_address)
            .set(value - amount_collateral);

        evm::log(CollateralRedeemed {
            redeemedFrom: from,
            redeemedTo: to,
            amount: amount_collateral,
            token: token_collateral_address,
        });
        let token = IERC20::new(token_collateral_address);
        if token.transfer(Call::new(), to, amount_collateral).is_err() {
            Err(DSCEngineError::TransferFailed(TransferFailed {}))
        } else {
            Ok(())
        }
    }

    pub fn redeem_collateral_for_dsc(
        &mut self,
        token_collateral_address: Address,
        amount_collateral: U256,
        amount_dsc_to_burn: U256,
    ) -> Result<(), DSCEngineError> {
        self.more_than_zero(amount_collateral)?;
        self.is_allowed_token(token_collateral_address)?;

        self._burn_dsc(amount_dsc_to_burn, msg::sender(), msg::sender());
        let _ = self._redeem_collateral(
            token_collateral_address,
            amount_collateral,
            msg::sender(),
            msg::sender(),
        );
        self._revert_if_health_factor_is_broken(msg::sender())?;
        Ok(())
    }

    pub fn redeem_collateral(
        &mut self,
        token_collateral_address: Address,
        amount_collateral: U256,
    ) -> Result<(), DSCEngineError> {
        self.more_than_zero(amount_collateral)?;
        // Note: nonReentrant is not directly available in Rust, consider using a reentrancy guard if needed

        let _ = self._redeem_collateral(
            token_collateral_address,
            amount_collateral,
            msg::sender(),
            msg::sender(),
        );
        self._revert_if_health_factor_is_broken(msg::sender())?;
        Ok(())
    }

    pub fn mint_dsc(&mut self, amount_dsc_to_mint: U256) -> Result<(), DSCEngineError> {
        self.more_than_zero(amount_dsc_to_mint)?;
        // Note: nonReentrant is not directly available in Rust, consider using a reentrancy guard if needed
        let user_dsc_minted = self.dsc_minted.get(msg::sender());
        self.dsc_minted
            .setter(msg::sender())
            .set(user_dsc_minted + amount_dsc_to_mint);
        self._revert_if_health_factor_is_broken(msg::sender())?;
        self.dsc
            .mint(msg::sender(), amount_dsc_to_mint)
            .map_err(|e| DSCEngineError::DecentralizedStableCoinError(e))?;
        Ok(())
    }

    pub fn burn_dsc(&mut self, amount: U256) -> Result<(), DSCEngineError> {
        self.more_than_zero(amount)?;
        self.dsc
            .burn(amount)
            .map_err(|e| DSCEngineError::DecentralizedStableCoinError(e))?;
        // ... 其他逻辑
        Ok(())
    }

    pub fn liquidate(
        &mut self,
        collateral: Address,
        user: Address,
        debt_to_cover: U256,
    ) -> Result<(), DSCEngineError> {
        self.more_than_zero(debt_to_cover)?;
        // Note: nonReentrant is not directly available in Rust, consider using a reentrancy guard if needed

        let starting_user_health_factor = self._health_factor(user);
        if starting_user_health_factor >= self.min_health_factor.get() {
            return Err(DSCEngineError::HealthFactorOk(HealthFactorOk {}));
        }

        let token_amount_from_debt_covered =
            self.get_token_amount_from_usd(collateral, debt_to_cover);
        let bonus_collateral =
            (token_amount_from_debt_covered * self.liquidation_bonus.get()) / U256::from(100);
        let total_collateral_to_redeem = token_amount_from_debt_covered + bonus_collateral;

        let _ =
            self._redeem_collateral(collateral, total_collateral_to_redeem, user, msg::sender());
        self._burn_dsc(debt_to_cover, user, msg::sender());

        let ending_user_health_factor = self._health_factor(user);
        if ending_user_health_factor <= starting_user_health_factor {
            return Err(DSCEngineError::HealthFactorNotImproved(
                HealthFactorNotImproved {},
            ));
        }
        self._revert_if_health_factor_is_broken(msg::sender())?;
        Ok(())
    }

    // Helper functions
    fn more_than_zero(&self, amount: U256) -> Result<(), DSCEngineError> {
        if amount == U256::ZERO {
            Err(DSCEngineError::NeedsMoreThanZero(NeedsMoreThanZero {}))
        } else {
            Ok(())
        }
    }

    fn is_allowed_token(&self, token: Address) -> Result<(), DSCEngineError> {
        if self.price_feeds.get(token).is_zero() {
            Err(DSCEngineError::NotAllowedToken(NotAllowedToken {}))
        } else {
            Ok(())
        }
    }

    fn _revert_if_health_factor_is_broken(&self, user: Address) -> Result<(), DSCEngineError> {
        let user_health_factor = self._health_factor(user);
        if user_health_factor < self.min_health_factor.get() {
            return Err(DSCEngineError::BreaksHealthFactor(BreaksHealthFactor {
                _0: user_health_factor,
            }));
        }
        Ok(())
    }

    fn _health_factor(&self, user: Address) -> U256 {
        let (total_dsc_minted, collateral_value_in_usd) = self._get_account_info(user);
        self._calculate_health_factor(total_dsc_minted, collateral_value_in_usd)
    }

    fn _calculate_health_factor(
        &self,
        total_dsc_minted: U256,
        collateral_value_in_usd: U256,
    ) -> U256 {
        if total_dsc_minted == U256::ZERO {
            return U256::MAX;
        }
        let collateral_adjusted_for_threshold = (collateral_value_in_usd
            * self.liquidation_threshold.get())
            / self.liquidation_precision.get();
        (collateral_adjusted_for_threshold * self.precision.get()) / total_dsc_minted
    }

    fn _get_account_info(&self, user: Address) -> (U256, U256) {
        let total_dsc_minted = self.dsc_minted.get(user);
        let collateral_value_in_usd = self.get_account_collateral_value_in_usd(user);
        (total_dsc_minted, collateral_value_in_usd)
    }

    /* pub fn calculate_health_factor(
        &self,
        total_dsc_minted: U256,
        collateral_value_in_usd: U256,
    ) -> U256 {
        self._calculate_health_factor(total_dsc_minted, collateral_value_in_usd)
    } */

    pub fn get_token_amount_from_usd(&self, token: Address, usd_amount_in_wei: U256) -> U256 {
        let price_feed = IAggregatorV3::new(self.price_feeds.get(token));
        let (_, price, _, _, _) = match price_feed.latest_round_data(Call::new()) {
            Ok(data) => data,
            Err(_) => return U256::ZERO,
        };

        let price_u256 = match U256::try_from(price) {
            Ok(price) => price,
            Err(_) => return U256::ZERO,
        };

        (usd_amount_in_wei * self.precision.get())
            / (price_u256 * self.additional_feed_precision.get())
    }

    pub fn get_account_collateral_value_in_usd(&self, user: Address) -> U256 {
        let mut total_collateral_value_in_usd = U256::ZERO;
        for i in 0..self.collateral_tokens.len() {
            let token_option = self.collateral_tokens.get(i);
            match token_option {
                Some(token) => {
                    let amount = self.collateral_deposited.getter(user).get(token);
                    total_collateral_value_in_usd += self.get_usd_value(token, amount);
                }
                None => (),
            }
        }
        total_collateral_value_in_usd
    }

    pub fn get_usd_value(&self, token: Address, amount: U256) -> U256 {
        let price_feed = IAggregatorV3::new(self.price_feeds.get(token));
        let (_, price, _, _, _) = match price_feed.latest_round_data(Call::new()) {
            Ok(data) => data,
            Err(_) => return U256::ZERO,
        };

        let price_u256 = match U256::try_from(price) {
            Ok(price) => price,
            Err(_) => return U256::ZERO,
        };
        ((price_u256 * self.additional_feed_precision.get()) * amount) / self.precision.get()
    }

    /* pub fn get_account_info(&self, user: Address) -> (U256, U256) {
        self._get_account_info(user)
    } */

    pub fn get_collateral_tokens(&self) -> Vec<Address> {
        let mut tokens = Vec::new();
        for i in 0..self.collateral_tokens.len() {
            tokens.push(self.collateral_tokens.get(i));
        }
        // Filter out any None values
        tokens
            .into_iter()
            .filter_map(|opt_address| opt_address)
            .collect()
    }

    pub fn get_collateral_balance_of_user(&self, user: Address, token: Address) -> U256 {
        self.collateral_deposited.getter(user).get(token)
    }

    pub fn get_additional_feed_precision(&self) -> U256 {
        self.additional_feed_precision.get()
    }

    pub fn get_precision(&self) -> U256 {
        self.precision.get()
    }

    pub fn get_health_factor(&self, user: Address) -> U256 {
        self._health_factor(user)
    }

    pub fn get_liquidation_bonus(&self) -> U256 {
        self.liquidation_bonus.get()
    }

    pub fn get_collateral_token_price_feed(&self, token: Address) -> Address {
        self.price_feeds.get(token)
    }
}

impl MethodError for DSCEngineError {
    fn encode(self) -> Vec<u8> {
        From::from(self)
    }
}
