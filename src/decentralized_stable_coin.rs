use alloy_primitives::{Address, U256};
use alloy_sol_types::sol;
use stylus_sdk::{call::MethodError, msg, prelude::*, storage::StorageAddress};

use crate::erc20::{Erc20, Erc20Error, Erc20Params};

sol! {
    error MustBeMoreThanZero();
    error BurnAmountExceedsBalance();
    error NotZeroAddress();
    error UnknownError();
    error NotOwner();
}

sol_storage! {
    pub struct DecentralizedStableCoin {
        #[borrow]
        Erc20<StylusTokenParams> erc20;
        address owner;
    }
}

/// Immutable definitions
pub struct StylusTokenParams;
impl Erc20Params for StylusTokenParams {
    const NAME: &'static str = "DecentralizedStableCoin";
    const SYMBOL: &'static str = "DSC";
    const DECIMALS: u8 = 18;
}

#[derive(SolidityError)]
pub enum DecentralizedStableCoinError {
    MustBeMoreThanZero(MustBeMoreThanZero),
    BurnAmountExceedsBalance(BurnAmountExceedsBalance),
    NotZeroAddress(NotZeroAddress),
    UnknownError(UnknownError),
    NotOwner(NotOwner),
    Erc20Error(Erc20Error),
}

impl MethodError for DecentralizedStableCoinError {
    fn encode(self) -> Vec<u8> {
        From::from(self)
    }
}

#[public]
impl DecentralizedStableCoin {
    pub fn constructor(&mut self) {
        self.owner.set(msg::sender());
    }

    pub fn new(owner: Address) -> Result<(), DecentralizedStableCoinError> {
        let mut instance = Self::default();
        instance.owner.set(owner);
        Ok(())
    }

    pub fn burn(&mut self, amount: U256) -> Result<(), DecentralizedStableCoinError> {
        self.only_owner()?;

        if amount == U256::ZERO {
            return Err(DecentralizedStableCoinError::MustBeMoreThanZero(
                MustBeMoreThanZero {},
            ));
        }

        let balance = self.erc20.balance_of(msg::sender());
        if amount > balance {
            return Err(DecentralizedStableCoinError::BurnAmountExceedsBalance(
                BurnAmountExceedsBalance {},
            ));
        }

        self.erc20
            .burn(msg::sender(), amount)
            .map_err(|e| match e {
                Erc20Error::InsufficientBalance(_) => {
                    DecentralizedStableCoinError::BurnAmountExceedsBalance(
                        BurnAmountExceedsBalance {},
                    )
                }
                _ => DecentralizedStableCoinError::UnknownError(UnknownError {}),
            })?;
        Ok(())
    }

    pub fn mint(
        &mut self,
        to: Address,
        amount: U256,
    ) -> Result<bool, DecentralizedStableCoinError> {
        self.only_owner()?;

        if amount == U256::ZERO {
            return Err(DecentralizedStableCoinError::MustBeMoreThanZero(
                MustBeMoreThanZero {},
            ));
        }
        if to == Address::ZERO {
            return Err(DecentralizedStableCoinError::NotZeroAddress(
                NotZeroAddress {},
            ));
        }

        self.erc20
            .mint(to, amount)
            .map_err(|_| DecentralizedStableCoinError::UnknownError(UnknownError {}))?;
        Ok(true)
    }

    fn only_owner(&self) -> Result<(), DecentralizedStableCoinError> {
        if msg::sender() != self.owner.get() {
            return Err(DecentralizedStableCoinError::NotOwner(NotOwner {}));
        }
        Ok(())
    }

    pub fn transfer_from(
        &mut self,
        from: Address,
        to: Address,
        value: U256,
    ) -> Result<bool, DecentralizedStableCoinError> {
        self.erc20
            .transfer_from(from, to, value)
            .map_err(DecentralizedStableCoinError::Erc20Error)
    }
}

impl Default for DecentralizedStableCoin {
    fn default() -> Self {
        unsafe {
            Self {
                erc20: Erc20::default(),
                owner: StorageAddress::new(U256::from(0), u8::from(0)),
            }
        }
    }
}
