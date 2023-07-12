pub use payment_sweeper::*;
/// This module was auto-generated with ethers-rs Abigen.
/// More information at: <https://github.com/gakonst/ethers-rs>
#[allow(
    clippy::enum_variant_names,
    clippy::too_many_arguments,
    clippy::upper_case_acronyms,
    clippy::type_complexity,
    dead_code,
    non_camel_case_types,
)]
pub mod payment_sweeper {
    #[rustfmt::skip]
    const __ABI: &str = "[\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"constructor\",\n        \"inputs\": [\n            {\n                \"name\": \"_factory\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"send_token\",\n        \"inputs\": [\n            {\n                \"name\": \"_token\",\n                \"type\": \"address\"\n            },\n            {\n                \"name\": \"_amount\",\n                \"type\": \"uint256\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"sweep_token_balance\",\n        \"inputs\": [\n            {\n                \"name\": \"_token\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"recover_token_balance\",\n        \"inputs\": [\n            {\n                \"name\": \"_token\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"FACTORY\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    }\n]";
    ///The parsed JSON ABI of the contract.
    pub static PAYMENTSWEEPER_ABI: ::ethers::contract::Lazy<::ethers::core::abi::Abi> = ::ethers::contract::Lazy::new(||
    ::ethers::core::utils::__serde_json::from_str(__ABI).expect("ABI is always valid"));
    pub struct PaymentSweeper<M>(::ethers::contract::Contract<M>);
    impl<M> ::core::clone::Clone for PaymentSweeper<M> {
        fn clone(&self) -> Self {
            Self(::core::clone::Clone::clone(&self.0))
        }
    }
    impl<M> ::core::ops::Deref for PaymentSweeper<M> {
        type Target = ::ethers::contract::Contract<M>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl<M> ::core::ops::DerefMut for PaymentSweeper<M> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
    impl<M> ::core::fmt::Debug for PaymentSweeper<M> {
        fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
            f.debug_tuple(stringify!(PaymentSweeper)).field(&self.address()).finish()
        }
    }
    impl<M: ::ethers::providers::Middleware> PaymentSweeper<M> {
        /// Creates a new contract instance with the specified `ethers` client at
        /// `address`. The contract derefs to a `ethers::Contract` object.
        pub fn new<T: Into<::ethers::core::types::Address>>(
            address: T,
            client: ::std::sync::Arc<M>,
        ) -> Self {
            Self(
                ::ethers::contract::Contract::new(
                    address.into(),
                    PAYMENTSWEEPER_ABI.clone(),
                    client,
                ),
            )
        }
        ///Calls the contract's `FACTORY` (0x2dd31000) function
        pub fn factory(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([45, 211, 16, 0], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `recover_token_balance` (0xfb2b77ff) function
        pub fn recover_token_balance(
            &self,
            token: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([251, 43, 119, 255], token)
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `send_token` (0x315a0826) function
        pub fn send_token(
            &self,
            token: ::ethers::core::types::Address,
            amount: ::ethers::core::types::U256,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([49, 90, 8, 38], (token, amount))
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `sweep_token_balance` (0x0d7c94a1) function
        pub fn sweep_token_balance(
            &self,
            token: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([13, 124, 148, 161], token)
                .expect("method not found (this should never happen)")
        }
    }
    impl<M: ::ethers::providers::Middleware> From<::ethers::contract::Contract<M>>
    for PaymentSweeper<M> {
        fn from(contract: ::ethers::contract::Contract<M>) -> Self {
            Self::new(contract.address(), contract.client())
        }
    }
    ///Container type for all input parameters for the `FACTORY` function with signature `FACTORY()` and selector `0x2dd31000`
    #[derive(
        Clone,
        ::ethers::contract::EthCall,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethcall(name = "FACTORY", abi = "FACTORY()")]
    pub struct FactoryCall;
    ///Container type for all input parameters for the `recover_token_balance` function with signature `recover_token_balance(address)` and selector `0xfb2b77ff`
    #[derive(
        Clone,
        ::ethers::contract::EthCall,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethcall(name = "recover_token_balance", abi = "recover_token_balance(address)")]
    pub struct RecoverTokenBalanceCall {
        pub token: ::ethers::core::types::Address,
    }
    ///Container type for all input parameters for the `send_token` function with signature `send_token(address,uint256)` and selector `0x315a0826`
    #[derive(
        Clone,
        ::ethers::contract::EthCall,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethcall(name = "send_token", abi = "send_token(address,uint256)")]
    pub struct SendTokenCall {
        pub token: ::ethers::core::types::Address,
        pub amount: ::ethers::core::types::U256,
    }
    ///Container type for all input parameters for the `sweep_token_balance` function with signature `sweep_token_balance(address)` and selector `0x0d7c94a1`
    #[derive(
        Clone,
        ::ethers::contract::EthCall,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethcall(name = "sweep_token_balance", abi = "sweep_token_balance(address)")]
    pub struct SweepTokenBalanceCall {
        pub token: ::ethers::core::types::Address,
    }
    ///Container type for all of the contract's call
    #[derive(Clone, ::ethers::contract::EthAbiType, Debug, PartialEq, Eq, Hash)]
    pub enum PaymentSweeperCalls {
        Factory(FactoryCall),
        RecoverTokenBalance(RecoverTokenBalanceCall),
        SendToken(SendTokenCall),
        SweepTokenBalance(SweepTokenBalanceCall),
    }
    impl ::ethers::core::abi::AbiDecode for PaymentSweeperCalls {
        fn decode(
            data: impl AsRef<[u8]>,
        ) -> ::core::result::Result<Self, ::ethers::core::abi::AbiError> {
            let data = data.as_ref();
            if let Ok(decoded)
                = <FactoryCall as ::ethers::core::abi::AbiDecode>::decode(data) {
                return Ok(Self::Factory(decoded));
            }
            if let Ok(decoded)
                = <RecoverTokenBalanceCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::RecoverTokenBalance(decoded));
            }
            if let Ok(decoded)
                = <SendTokenCall as ::ethers::core::abi::AbiDecode>::decode(data) {
                return Ok(Self::SendToken(decoded));
            }
            if let Ok(decoded)
                = <SweepTokenBalanceCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::SweepTokenBalance(decoded));
            }
            Err(::ethers::core::abi::Error::InvalidData.into())
        }
    }
    impl ::ethers::core::abi::AbiEncode for PaymentSweeperCalls {
        fn encode(self) -> Vec<u8> {
            match self {
                Self::Factory(element) => ::ethers::core::abi::AbiEncode::encode(element),
                Self::RecoverTokenBalance(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::SendToken(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::SweepTokenBalance(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
            }
        }
    }
    impl ::core::fmt::Display for PaymentSweeperCalls {
        fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
            match self {
                Self::Factory(element) => ::core::fmt::Display::fmt(element, f),
                Self::RecoverTokenBalance(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::SendToken(element) => ::core::fmt::Display::fmt(element, f),
                Self::SweepTokenBalance(element) => ::core::fmt::Display::fmt(element, f),
            }
        }
    }
    impl ::core::convert::From<FactoryCall> for PaymentSweeperCalls {
        fn from(value: FactoryCall) -> Self {
            Self::Factory(value)
        }
    }
    impl ::core::convert::From<RecoverTokenBalanceCall> for PaymentSweeperCalls {
        fn from(value: RecoverTokenBalanceCall) -> Self {
            Self::RecoverTokenBalance(value)
        }
    }
    impl ::core::convert::From<SendTokenCall> for PaymentSweeperCalls {
        fn from(value: SendTokenCall) -> Self {
            Self::SendToken(value)
        }
    }
    impl ::core::convert::From<SweepTokenBalanceCall> for PaymentSweeperCalls {
        fn from(value: SweepTokenBalanceCall) -> Self {
            Self::SweepTokenBalance(value)
        }
    }
    ///Container type for all return fields from the `FACTORY` function with signature `FACTORY()` and selector `0x2dd31000`
    #[derive(
        Clone,
        ::ethers::contract::EthAbiType,
        ::ethers::contract::EthAbiCodec,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    pub struct FactoryReturn(pub ::ethers::core::types::Address);
}
