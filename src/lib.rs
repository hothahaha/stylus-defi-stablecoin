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
    contract, evm, msg,
    prelude::*,
};

sol! {
    // 抵押品存入事件：记录用户存入抵押品的信息
    event CollateralDeposited(address indexed user, address indexed token, uint256 amount);
    // 抵押品赎回事件：记录抵押品赎回的信息
    event CollateralRedeemed(
        address indexed redeemedFrom, address indexed redeemedTo, uint256 indexed amount, address token
    );

    // 错误定义
    error TokenAddressesAndPriceFeedAddressesMustBeSameLength(); // 代币地址和价格预言机地址长度不匹配错误
    error NeedsMoreThanZero();                                   // 数量必须大于零错误
    error NotAllowedToken();                                     // 不支持的代币错误
    error TransferFailed();                                      // 转账失败错误
    error BreaksHealthFactor(uint256);                          // 健康因子不足错误
    error MintFailed();                                         // 铸造失败错误
    error HealthFactorOk();                                     // 健康因子正常错误（不需要清算）
    error HealthFactorNotImproved();                           // 健康因子未改善错误
    error PriceFeedError();                                    // 价格预言机错误
    error ConversionError();                                   // 数据转换错误
}

// Assuming we have these imports available
#[derive(SolidityError)]
pub enum DSCEngineError {
    NeedsMoreThanZero(NeedsMoreThanZero), // 数量为零错误
    TokenAddressesAndPriceFeedAddressesMustBeSameLength(
        // 地址长度不匹配错误
        TokenAddressesAndPriceFeedAddressesMustBeSameLength,
    ),
    NotAllowedToken(NotAllowedToken),       // 不支持的代币错误
    TransferFailed(TransferFailed),         // 转账失败错误
    BreaksHealthFactor(BreaksHealthFactor), // 健康因子不足错误
    MintFailed(MintFailed),                 // 铸造失败错误
    HealthFactorOk(HealthFactorOk),         // 健康因子正常错误
    HealthFactorNotImproved(HealthFactorNotImproved), // 健康因子未改善错误
    PriceFeedError(PriceFeedError),         // 价格预言机错误
    ConversionError(ConversionError),       // 数据转换错误
    DecentralizedStableCoinError(DecentralizedStableCoinError), // 稳定币合约错误
}

sol_interface! {
    // 定义预言机接口：用于获取价格数据
    interface IAggregatorV3 {
        // 获取最新一轮的价格数据
        function latestRoundData()
    external
    view
    returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
    }
    // 定义 ERC20 代币接口
    interface IERC20 {
        // 从指定地址转账到目标地址
        function transfer(address to, uint256 value) external returns (bool);
        // 转账到目标地址
        function transferFrom(address from, address to, uint256 value) external returns (bool);
    }
}

// 定义合约存储结构
sol_storage! {
    #[entrypoint]
    pub struct DSCEngine {
        uint256 additional_feed_precision;    // 预言机精度调整因子：用于调整价格精度
        uint256 precision;                    // 基础精度：合约基础计算精度
        uint256 liquidation_threshold;        // 清算阈值：触发清算的阈值
        uint256 liquidation_precision;        // 清算精度：清算计算精度
        uint256 min_health_factor;           // 最小健康因子：维持仓位所需的最小健康因子
        uint256 liquidation_bonus;           // 清算奖励：清算人获得的奖励比例
        mapping(address => address) price_feeds;  // 价格预言机映射：代币地址到预言机地址的映射
        mapping(address => mapping(address => uint256)) collateral_deposited;  // 抵押品存款映射：用户地址到代币地址到数量的映射
        mapping(address => uint256) dsc_minted;   // 已铸造映射：用户地址到已铸造稳定币数量的映射
        address[] collateral_tokens;          // 抵押品列表：支持的抵押品代币地址列表
        DecentralizedStableCoin dsc;         // DSC实例：稳定币合约实例
    }
}

#[public]
impl DSCEngine {
    pub fn constructor(
        &mut self,
        token_addresses: Vec<Address>,      // 支持的代币地址列表
        price_feed_addresses: Vec<Address>, // 对应的价格预言机地址列表
    ) -> Result<(), DSCEngineError> {
        // 检查代币地址和价格预言机地址长度是否匹配
        if token_addresses.len() != price_feed_addresses.len() {
            return Err(
                DSCEngineError::TokenAddressesAndPriceFeedAddressesMustBeSameLength(
                    TokenAddressesAndPriceFeedAddressesMustBeSameLength {},
                ),
            );
        }
        // 初始化价格预言机映射
        for (token, price_feed) in token_addresses.iter().zip(price_feed_addresses.iter()) {
            self.price_feeds.insert(*token, *price_feed);
            self.collateral_tokens.push(*token);
        }

        let mut dsc: DecentralizedStableCoin = DecentralizedStableCoin::default();
        dsc.constructor();
        self.dsc = dsc;

        self.additional_feed_precision
            .set(U256::from(10).pow(U256::from(10))); // 设置精度
        self.precision.set(U256::from(10).pow(U256::from(18)));
        self.liquidation_threshold.set(U256::from(50)); // 设置清算阈值
        self.liquidation_precision.set(U256::from(100)); // 设置清算精度
        self.min_health_factor
            .set(U256::from(10).pow(U256::from(18))); // 设置最小健康因子
        self.liquidation_bonus.set(U256::from(10)); // 设置清算奖励
        Ok(())
    }

    /// 存入抵押品并铸造稳定币
    pub fn deposit_collateral_and_mint_dsc(
        &mut self,
        token_collateral_address: Address, // 抵押品地址
        amount_collateral: U256,           // 抵押品数量
        amount_dsc_to_mint: U256,          // 要铸造的稳定币数量
    ) -> Result<(), DSCEngineError> {
        let _ = self.deposit_collateral(token_collateral_address, amount_collateral);
        self.mint_dsc(amount_dsc_to_mint)?;
        Ok(())
    }

    /// 存入抵押品
    pub fn deposit_collateral(
        &mut self,
        token_collateral_address: Address,
        amount_collateral: U256,
    ) -> Result<(), DSCEngineError> {
        // 检查抵押品数量是否大于零
        if amount_collateral == U256::ZERO {
            return Err(DSCEngineError::NeedsMoreThanZero(NeedsMoreThanZero {}));
        }
        // 检查代币是否在支持列表中
        if self.price_feeds.get(token_collateral_address).is_zero() {
            return Err(DSCEngineError::NotAllowedToken(NotAllowedToken {}));
        }

        let sender = msg::sender();
        // 获取用户抵押品存款映射
        let user_collateral_mapping = self.collateral_deposited.getter(sender);
        // 获取用户特定代币的抵押品数量
        let user_collateral = user_collateral_mapping.getter(token_collateral_address);
        let value = user_collateral.get();
        // 更新用户抵押品存款映射
        self.collateral_deposited
            .setter(sender)
            .setter(token_collateral_address)
            .set(value + amount_collateral);

        // 记录抵押品存入事件
        evm::log(CollateralDeposited {
            user: sender,
            token: token_collateral_address,
            amount: amount_collateral,
        });

        let token = IERC20::new(token_collateral_address);
        // 从用户地址转账到合约地址
        if token
            .transfer_from(Call::new(), sender, contract::address(), amount_collateral)
            .is_err()
        {
            return Err(DSCEngineError::TransferFailed(TransferFailed {}));
        }
        Ok(())
    }

    /// 赎回抵押品并销毁稳定币
    pub fn redeem_collateral_for_dsc(
        &mut self,
        token_collateral_address: Address, // 抵押品地址
        amount_collateral: U256,           // 抵押品数量
        amount_dsc_to_burn: U256,          // 要销毁的稳定币数量
    ) -> Result<(), DSCEngineError> {
        // 检查抵押品数量是否大于零
        self.more_than_zero(amount_collateral)?;
        // 检查代币是否在支持列表中
        self.is_allowed_token(token_collateral_address)?;
        // 销毁稳定币
        self._burn_dsc(amount_dsc_to_burn, msg::sender(), msg::sender());
        // 赎回抵押品
        let _ = self._redeem_collateral(
            token_collateral_address,
            amount_collateral,
            msg::sender(),
            msg::sender(),
        );
        self._revert_if_health_factor_is_broken(msg::sender())?;
        Ok(())
    }

    /// 赎回抵押品
    pub fn redeem_collateral(
        &mut self,
        token_collateral_address: Address, // 抵押品地址
        amount_collateral: U256,           // 抵押品数量
    ) -> Result<(), DSCEngineError> {
        self.more_than_zero(amount_collateral)?;
        // 赎回抵押品
        let _ = self._redeem_collateral(
            token_collateral_address,
            amount_collateral,
            msg::sender(),
            msg::sender(),
        );
        self._revert_if_health_factor_is_broken(msg::sender())?;
        Ok(())
    }

    /// 铸造稳定币
    pub fn mint_dsc(
        &mut self,
        amount_dsc_to_mint: U256, // 要铸造的稳定币数量
    ) -> Result<(), DSCEngineError> {
        // 检查铸造数量是否大于零
        self.more_than_zero(amount_dsc_to_mint)?;
        // 获取用户已铸造的稳定币数量
        let user_dsc_minted = self.dsc_minted.get(msg::sender());
        // 更新用户已铸造的稳定币数量
        self.dsc_minted
            .setter(msg::sender())
            .set(user_dsc_minted + amount_dsc_to_mint);
        // 检查健康因子是否正常
        self._revert_if_health_factor_is_broken(msg::sender())?;
        // 铸造稳定币
        self.dsc
            .mint(msg::sender(), amount_dsc_to_mint)
            .map_err(|e| DSCEngineError::DecentralizedStableCoinError(e))?;
        Ok(())
    }

    pub fn burn_dsc(
        &mut self,
        amount: U256, // 要销毁的稳定币数量
    ) -> Result<(), DSCEngineError> {
        self.more_than_zero(amount)?;
        self.dsc
            .burn(amount)
            .map_err(|e| DSCEngineError::DecentralizedStableCoinError(e))?;
        // ... 其他逻辑
        Ok(())
    }

    /// 清算功能
    pub fn liquidate(
        &mut self,
        collateral: Address, // 抵押品地址
        user: Address,       // 要清算的用户地址
        debt_to_cover: U256, // 要清算的债务数量
    ) -> Result<(), DSCEngineError> {
        // 检查债务数量是否大于零
        self.more_than_zero(debt_to_cover)?;
        // 检查健康因子是否正常
        let starting_user_health_factor = self._health_factor(user);
        if starting_user_health_factor >= self.min_health_factor.get() {
            return Err(DSCEngineError::HealthFactorOk(HealthFactorOk {}));
        }
        // 获取债务对应的抵押品数量
        let token_amount_from_debt_covered =
            self.get_token_amount_from_usd(collateral, debt_to_cover);
        // 计算清算奖励
        let bonus_collateral =
            (token_amount_from_debt_covered * self.liquidation_bonus.get()) / U256::from(100);
        let total_collateral_to_redeem = token_amount_from_debt_covered + bonus_collateral;
        // 赎回抵押品
        let _ =
            self._redeem_collateral(collateral, total_collateral_to_redeem, user, msg::sender());
        // 销毁稳定币
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

    // 内部辅助函数
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

    // 销毁稳定币的内部实现
    fn _burn_dsc(&mut self, amount_dsc_to_burn: U256, on_behalf_of: Address, dsc_from: Address) {
        // 获取用户已铸造的稳定币数量
        let user_dsc_minted = self.dsc_minted.getter(on_behalf_of);
        let value = user_dsc_minted.get();
        // 更新用户已铸造的稳定币数量
        self.dsc_minted
            .setter(on_behalf_of)
            .set(value - amount_dsc_to_burn);
        // 从用户地址转账到合约地址
        if !self
            .dsc
            .transfer_from(dsc_from, contract::address(), amount_dsc_to_burn)
            .is_err()
        {
            panic!("TransferFailed");
        }
        // 销毁稳定币
        let _ = self.dsc.burn(amount_dsc_to_burn);
    }

    // 赎回抵押品的内部实现
    fn _redeem_collateral(
        &mut self,
        token_collateral_address: Address, // 抵押品地址
        amount_collateral: U256,           // 抵押品数量
        from: Address,                     // 赎回者地址
        to: Address,                       // 接收者地址
    ) -> Result<(), DSCEngineError> {
        // 获取用户抵押品存款映射
        let user_collateral_mapping = self.collateral_deposited.getter(from);
        // 获取用户特定代币的抵押品数量
        let user_collateral = user_collateral_mapping.getter(token_collateral_address);
        let value = user_collateral.get();
        // 更新用户抵押品存款映射
        self.collateral_deposited
            .setter(from)
            .setter(token_collateral_address)
            .set(value - amount_collateral);
        // 记录抵押品赎回事件
        evm::log(CollateralRedeemed {
            redeemedFrom: from,
            redeemedTo: to,
            amount: amount_collateral,
            token: token_collateral_address,
        });
        // 获取代币实例
        let token = IERC20::new(token_collateral_address);
        // 从合约地址转账到接收者地址
        if token.transfer(Call::new(), to, amount_collateral).is_err() {
            Err(DSCEngineError::TransferFailed(TransferFailed {}))
        } else {
            Ok(())
        }
    }

    // 检查健康因子是否正常
    fn _revert_if_health_factor_is_broken(&self, user: Address) -> Result<(), DSCEngineError> {
        // 获取用户健康因子
        let user_health_factor = self._health_factor(user);
        // 检查健康因子是否低于最小值
        if user_health_factor < self.min_health_factor.get() {
            return Err(DSCEngineError::BreaksHealthFactor(BreaksHealthFactor {
                _0: user_health_factor,
            }));
        }
        Ok(())
    }

    // 获取用户健康因子
    fn _health_factor(&self, user: Address) -> U256 {
        // 获取用户账户信息
        let (total_dsc_minted, collateral_value_in_usd) = self._get_account_info(user);
        // 计算健康因子
        self._calculate_health_factor(total_dsc_minted, collateral_value_in_usd)
    }

    // 计算健康因子
    fn _calculate_health_factor(
        &self,
        total_dsc_minted: U256,
        collateral_value_in_usd: U256,
    ) -> U256 {
        // 检查稳定币铸造数量是否为零
        if total_dsc_minted == U256::ZERO {
            return U256::MAX;
        }
        // 计算抵押品调整值
        let collateral_adjusted_for_threshold = (collateral_value_in_usd
            * self.liquidation_threshold.get())
            / self.liquidation_precision.get();
        // 计算健康因子
        (collateral_adjusted_for_threshold * self.precision.get()) / total_dsc_minted
    }

    // 获取用户账户信息
    fn _get_account_info(&self, user: Address) -> (U256, U256) {
        // 获取用户已铸造的稳定币数量
        let total_dsc_minted = self.dsc_minted.get(user);
        // 获取用户账户抵押品总价值
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
        // 获取价格预言机实例
        let price_feed = IAggregatorV3::new(self.price_feeds.get(token));
        // 获取价格预言机最新数据
        let (_, price, _, _, _) = match price_feed.latest_round_data(Call::new()) {
            Ok(data) => data,
            Err(_) => return U256::ZERO,
        };
        // 将价格转换为 U256 类型
        let price_u256 = match U256::try_from(price) {
            Ok(price) => price,
            Err(_) => return U256::ZERO,
        };
        // 计算抵押品金额
        (usd_amount_in_wei * self.precision.get())
            / (price_u256 * self.additional_feed_precision.get())
    }

    pub fn get_account_collateral_value_in_usd(&self, user: Address) -> U256 {
        // 初始化抵押品总价值
        let mut total_collateral_value_in_usd = U256::ZERO;
        // 遍历所有抵押品
        for i in 0..self.collateral_tokens.len() {
            let token_option = self.collateral_tokens.get(i);
            match token_option {
                Some(token) => {
                    // 获取用户特定代币的抵押品数量
                    let amount = self.collateral_deposited.getter(user).get(token);
                    // 计算抵押品金额
                    total_collateral_value_in_usd += self.get_usd_value(token, amount);
                }
                None => (),
            }
        }
        total_collateral_value_in_usd
    }

    // 获取抵押品金额
    pub fn get_usd_value(&self, token: Address, amount: U256) -> U256 {
        // 获取价格预言机实例
        let price_feed = IAggregatorV3::new(self.price_feeds.get(token));
        // 获取价格预言机最新数据
        let (_, price, _, _, _) = match price_feed.latest_round_data(Call::new()) {
            Ok(data) => data,
            Err(_) => return U256::ZERO,
        };
        // 将价格转换为 U256 类型
        let price_u256 = match U256::try_from(price) {
            Ok(price) => price,
            Err(_) => return U256::ZERO,
        };
        // 计算抵押品金额
        ((price_u256 * self.additional_feed_precision.get()) * amount) / self.precision.get()
    }

    /* pub fn get_account_info(&self, user: Address) -> (U256, U256) {
        self._get_account_info(user)
    } */

    pub fn get_collateral_tokens(&self) -> Vec<Address> {
        // 初始化抵押品列表
        let mut tokens = Vec::new();
        // 遍历所有抵押品
        for i in 0..self.collateral_tokens.len() {
            tokens.push(self.collateral_tokens.get(i));
        }
        // 过滤掉 None 值
        tokens
            .into_iter()
            .filter_map(|opt_address| opt_address)
            .collect()
    }

    pub fn get_collateral_balance_of_user(&self, user: Address, token: Address) -> U256 {
        // 获取用户特定代币的抵押品数量
        self.collateral_deposited.getter(user).get(token)
    }

    pub fn get_additional_feed_precision(&self) -> U256 {
        // 获取精度调整因子
        self.additional_feed_precision.get()
    }

    pub fn get_precision(&self) -> U256 {
        // 获取基础精度
        self.precision.get()
    }

    pub fn get_health_factor(&self, user: Address) -> U256 {
        // 获取用户健康因子
        self._health_factor(user)
    }

    pub fn get_liquidation_bonus(&self) -> U256 {
        // 获取清算奖励
        self.liquidation_bonus.get()
    }

    pub fn get_collateral_token_price_feed(&self, token: Address) -> Address {
        // 获取价格预言机地址
        self.price_feeds.get(token)
    }
}

impl MethodError for DSCEngineError {
    fn encode(self) -> Vec<u8> {
        From::from(self)
    }
}
