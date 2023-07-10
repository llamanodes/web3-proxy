pub use payment_factory::*;
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
pub mod payment_factory {
    #[rustfmt::skip]
    const __ABI: &str = "[\n    {\n        \"name\": \"NewPaymentAddress\",\n        \"inputs\": [\n            {\n                \"name\": \"account\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"payment_address\",\n                \"type\": \"address\",\n                \"indexed\": false\n            }\n        ],\n        \"anonymous\": false,\n        \"type\": \"event\"\n    },\n    {\n        \"name\": \"PaymentReceived\",\n        \"inputs\": [\n            {\n                \"name\": \"account\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"token\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"amount\",\n                \"type\": \"uint256\",\n                \"indexed\": false\n            }\n        ],\n        \"anonymous\": false,\n        \"type\": \"event\"\n    },\n    {\n        \"name\": \"NewOwnerCommitted\",\n        \"inputs\": [\n            {\n                \"name\": \"owner\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"new_owner\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"finalize_time\",\n                \"type\": \"uint256\",\n                \"indexed\": false\n            }\n        ],\n        \"anonymous\": false,\n        \"type\": \"event\"\n    },\n    {\n        \"name\": \"NewOwnerAccepted\",\n        \"inputs\": [\n            {\n                \"name\": \"old_owner\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"owner\",\n                \"type\": \"address\",\n                \"indexed\": false\n            }\n        ],\n        \"anonymous\": false,\n        \"type\": \"event\"\n    },\n    {\n        \"name\": \"NewSweeperCommitted\",\n        \"inputs\": [\n            {\n                \"name\": \"sweeper\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"new_sweeper\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"finalize_time\",\n                \"type\": \"uint256\",\n                \"indexed\": false\n            }\n        ],\n        \"anonymous\": false,\n        \"type\": \"event\"\n    },\n    {\n        \"name\": \"NewSweeperSet\",\n        \"inputs\": [\n            {\n                \"name\": \"old_sweeper\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"sweeper\",\n                \"type\": \"address\",\n                \"indexed\": false\n            }\n        ],\n        \"anonymous\": false,\n        \"type\": \"event\"\n    },\n    {\n        \"name\": \"NewReceiverCommitted\",\n        \"inputs\": [\n            {\n                \"name\": \"receiver\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"new_receiver\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"finalize_time\",\n                \"type\": \"uint256\",\n                \"indexed\": false\n            }\n        ],\n        \"anonymous\": false,\n        \"type\": \"event\"\n    },\n    {\n        \"name\": \"NewReceiverSet\",\n        \"inputs\": [\n            {\n                \"name\": \"old_receiver\",\n                \"type\": \"address\",\n                \"indexed\": false\n            },\n            {\n                \"name\": \"receiver\",\n                \"type\": \"address\",\n                \"indexed\": false\n            }\n        ],\n        \"anonymous\": false,\n        \"type\": \"event\"\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"constructor\",\n        \"inputs\": [\n            {\n                \"name\": \"_owner\",\n                \"type\": \"address\"\n            },\n            {\n                \"name\": \"_receiver\",\n                \"type\": \"address\"\n            },\n            {\n                \"name\": \"_sweeper\",\n                \"type\": \"address\"\n            },\n            {\n                \"name\": \"_proxy\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"get_approved_tokens\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address[]\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"payment_received\",\n        \"inputs\": [\n            {\n                \"name\": \"_token\",\n                \"type\": \"address\"\n            },\n            {\n                \"name\": \"_amount\",\n                \"type\": \"uint256\"\n            }\n        ],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"bool\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"create_payment_address\",\n        \"inputs\": [],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"create_payment_address\",\n        \"inputs\": [\n            {\n                \"name\": \"_account\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"set_token_approvals\",\n        \"inputs\": [\n            {\n                \"name\": \"_tokens\",\n                \"type\": \"address[]\"\n            },\n            {\n                \"name\": \"_approved\",\n                \"type\": \"bool\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"commit_new_sweeper_implementation\",\n        \"inputs\": [\n            {\n                \"name\": \"_sweeper\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"finalize_new_sweeper_implementation\",\n        \"inputs\": [],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"commit_new_receiver\",\n        \"inputs\": [\n            {\n                \"name\": \"_receiver\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"finalize_new_receiver\",\n        \"inputs\": [],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"commit_transfer_ownership\",\n        \"inputs\": [\n            {\n                \"name\": \"_new_owner\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"nonpayable\",\n        \"type\": \"function\",\n        \"name\": \"accept_transfer_ownership\",\n        \"inputs\": [],\n        \"outputs\": []\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"PROXY_IMPLEMENTATION\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"sweeper_implementation\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"future_sweeper_implementation\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"owner\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"future_owner\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"receiver\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"future_receiver\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"transfer_ownership_timestamp\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"uint256\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"new_sweeper_timestamp\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"uint256\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"new_receiver_timestamp\",\n        \"inputs\": [],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"uint256\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"is_approved_token\",\n        \"inputs\": [\n            {\n                \"name\": \"arg0\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"bool\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"account_to_payment_address\",\n        \"inputs\": [\n            {\n                \"name\": \"arg0\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    },\n    {\n        \"stateMutability\": \"view\",\n        \"type\": \"function\",\n        \"name\": \"payment_address_to_account\",\n        \"inputs\": [\n            {\n                \"name\": \"arg0\",\n                \"type\": \"address\"\n            }\n        ],\n        \"outputs\": [\n            {\n                \"name\": \"\",\n                \"type\": \"address\"\n            }\n        ]\n    }\n]";
    ///The parsed JSON ABI of the contract.
    pub static PAYMENTFACTORY_ABI: ::ethers::contract::Lazy<::ethers::core::abi::Abi> = ::ethers::contract::Lazy::new(||
    ::ethers::core::utils::__serde_json::from_str(__ABI).expect("ABI is always valid"));
    pub struct PaymentFactory<M>(::ethers::contract::Contract<M>);
    impl<M> ::core::clone::Clone for PaymentFactory<M> {
        fn clone(&self) -> Self {
            Self(::core::clone::Clone::clone(&self.0))
        }
    }
    impl<M> ::core::ops::Deref for PaymentFactory<M> {
        type Target = ::ethers::contract::Contract<M>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl<M> ::core::ops::DerefMut for PaymentFactory<M> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
    impl<M> ::core::fmt::Debug for PaymentFactory<M> {
        fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
            f.debug_tuple(stringify!(PaymentFactory)).field(&self.address()).finish()
        }
    }
    impl<M: ::ethers::providers::Middleware> PaymentFactory<M> {
        /// Creates a new contract instance with the specified `ethers` client at
        /// `address`. The contract derefs to a `ethers::Contract` object.
        pub fn new<T: Into<::ethers::core::types::Address>>(
            address: T,
            client: ::std::sync::Arc<M>,
        ) -> Self {
            Self(
                ::ethers::contract::Contract::new(
                    address.into(),
                    PAYMENTFACTORY_ABI.clone(),
                    client,
                ),
            )
        }
        ///Calls the contract's `PROXY_IMPLEMENTATION` (0x5c7a7d99) function
        pub fn proxy_implementation(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([92, 122, 125, 153], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `accept_transfer_ownership` (0xe5ea47b8) function
        pub fn accept_transfer_ownership(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([229, 234, 71, 184], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `account_to_payment_address` (0x0b810230) function
        pub fn account_to_payment_address(
            &self,
            arg_0: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([11, 129, 2, 48], arg_0)
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `commit_new_receiver` (0x561a4147) function
        pub fn commit_new_receiver(
            &self,
            receiver: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([86, 26, 65, 71], receiver)
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `commit_new_sweeper_implementation` (0xafcbdc11) function
        pub fn commit_new_sweeper_implementation(
            &self,
            sweeper: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([175, 203, 220, 17], sweeper)
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `commit_transfer_ownership` (0x6b441a40) function
        pub fn commit_transfer_ownership(
            &self,
            new_owner: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([107, 68, 26, 64], new_owner)
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `create_payment_address` (0x6248d5c6) function
        pub fn create_payment_address(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([98, 72, 213, 198], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `create_payment_address` (0x998faa1e) function
        pub fn create_payment_address_with_account(
            &self,
            account: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([153, 143, 170, 30], account)
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `finalize_new_receiver` (0x73d02b33) function
        pub fn finalize_new_receiver(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([115, 208, 43, 51], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `finalize_new_sweeper_implementation` (0x41acefbb) function
        pub fn finalize_new_sweeper_implementation(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([65, 172, 239, 187], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `future_owner` (0x1ec0cdc1) function
        pub fn future_owner(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([30, 192, 205, 193], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `future_receiver` (0x3bea9ddd) function
        pub fn future_receiver(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([59, 234, 157, 221], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `future_sweeper_implementation` (0x4333d02b) function
        pub fn future_sweeper_implementation(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([67, 51, 208, 43], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `get_approved_tokens` (0x79aaf078) function
        pub fn get_approved_tokens(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::std::vec::Vec<::ethers::core::types::Address>,
        > {
            self.0
                .method_hash([121, 170, 240, 120], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `is_approved_token` (0x363b5b3e) function
        pub fn is_approved_token(
            &self,
            arg_0: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<M, bool> {
            self.0
                .method_hash([54, 59, 91, 62], arg_0)
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `new_receiver_timestamp` (0xf85c2e83) function
        pub fn new_receiver_timestamp(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<M, ::ethers::core::types::U256> {
            self.0
                .method_hash([248, 92, 46, 131], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `new_sweeper_timestamp` (0x05daefa8) function
        pub fn new_sweeper_timestamp(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<M, ::ethers::core::types::U256> {
            self.0
                .method_hash([5, 218, 239, 168], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `owner` (0x8da5cb5b) function
        pub fn owner(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([141, 165, 203, 91], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `payment_address_to_account` (0xe0934982) function
        pub fn payment_address_to_account(
            &self,
            arg_0: ::ethers::core::types::Address,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([224, 147, 73, 130], arg_0)
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `payment_received` (0x7414b098) function
        pub fn payment_received(
            &self,
            token: ::ethers::core::types::Address,
            amount: ::ethers::core::types::U256,
        ) -> ::ethers::contract::builders::ContractCall<M, bool> {
            self.0
                .method_hash([116, 20, 176, 152], (token, amount))
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `receiver` (0xf7260d3e) function
        pub fn receiver(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([247, 38, 13, 62], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `set_token_approvals` (0x50054612) function
        pub fn set_token_approvals(
            &self,
            tokens: ::std::vec::Vec<::ethers::core::types::Address>,
            approved: bool,
        ) -> ::ethers::contract::builders::ContractCall<M, ()> {
            self.0
                .method_hash([80, 5, 70, 18], (tokens, approved))
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `sweeper_implementation` (0x791cc746) function
        pub fn sweeper_implementation(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<
            M,
            ::ethers::core::types::Address,
        > {
            self.0
                .method_hash([121, 28, 199, 70], ())
                .expect("method not found (this should never happen)")
        }
        ///Calls the contract's `transfer_ownership_timestamp` (0x4598cb25) function
        pub fn transfer_ownership_timestamp(
            &self,
        ) -> ::ethers::contract::builders::ContractCall<M, ::ethers::core::types::U256> {
            self.0
                .method_hash([69, 152, 203, 37], ())
                .expect("method not found (this should never happen)")
        }
        ///Gets the contract's `NewOwnerAccepted` event
        pub fn new_owner_accepted_filter(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            NewOwnerAcceptedFilter,
        > {
            self.0.event()
        }
        ///Gets the contract's `NewOwnerCommitted` event
        pub fn new_owner_committed_filter(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            NewOwnerCommittedFilter,
        > {
            self.0.event()
        }
        ///Gets the contract's `NewPaymentAddress` event
        pub fn new_payment_address_filter(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            NewPaymentAddressFilter,
        > {
            self.0.event()
        }
        ///Gets the contract's `NewReceiverCommitted` event
        pub fn new_receiver_committed_filter(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            NewReceiverCommittedFilter,
        > {
            self.0.event()
        }
        ///Gets the contract's `NewReceiverSet` event
        pub fn new_receiver_set_filter(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            NewReceiverSetFilter,
        > {
            self.0.event()
        }
        ///Gets the contract's `NewSweeperCommitted` event
        pub fn new_sweeper_committed_filter(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            NewSweeperCommittedFilter,
        > {
            self.0.event()
        }
        ///Gets the contract's `NewSweeperSet` event
        pub fn new_sweeper_set_filter(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            NewSweeperSetFilter,
        > {
            self.0.event()
        }
        ///Gets the contract's `PaymentReceived` event
        pub fn payment_received_filter(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            PaymentReceivedFilter,
        > {
            self.0.event()
        }
        /// Returns an `Event` builder for all the events of this contract.
        pub fn events(
            &self,
        ) -> ::ethers::contract::builders::Event<
            ::std::sync::Arc<M>,
            M,
            PaymentFactoryEvents,
        > {
            self.0.event_with_filter(::core::default::Default::default())
        }
    }
    impl<M: ::ethers::providers::Middleware> From<::ethers::contract::Contract<M>>
    for PaymentFactory<M> {
        fn from(contract: ::ethers::contract::Contract<M>) -> Self {
            Self::new(contract.address(), contract.client())
        }
    }
    #[derive(
        Clone,
        ::ethers::contract::EthEvent,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethevent(name = "NewOwnerAccepted", abi = "NewOwnerAccepted(address,address)")]
    pub struct NewOwnerAcceptedFilter {
        pub old_owner: ::ethers::core::types::Address,
        pub owner: ::ethers::core::types::Address,
    }
    #[derive(
        Clone,
        ::ethers::contract::EthEvent,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethevent(
        name = "NewOwnerCommitted",
        abi = "NewOwnerCommitted(address,address,uint256)"
    )]
    pub struct NewOwnerCommittedFilter {
        pub owner: ::ethers::core::types::Address,
        pub new_owner: ::ethers::core::types::Address,
        pub finalize_time: ::ethers::core::types::U256,
    }
    #[derive(
        Clone,
        ::ethers::contract::EthEvent,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethevent(name = "NewPaymentAddress", abi = "NewPaymentAddress(address,address)")]
    pub struct NewPaymentAddressFilter {
        pub account: ::ethers::core::types::Address,
        pub payment_address: ::ethers::core::types::Address,
    }
    #[derive(
        Clone,
        ::ethers::contract::EthEvent,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethevent(
        name = "NewReceiverCommitted",
        abi = "NewReceiverCommitted(address,address,uint256)"
    )]
    pub struct NewReceiverCommittedFilter {
        pub receiver: ::ethers::core::types::Address,
        pub new_receiver: ::ethers::core::types::Address,
        pub finalize_time: ::ethers::core::types::U256,
    }
    #[derive(
        Clone,
        ::ethers::contract::EthEvent,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethevent(name = "NewReceiverSet", abi = "NewReceiverSet(address,address)")]
    pub struct NewReceiverSetFilter {
        pub old_receiver: ::ethers::core::types::Address,
        pub receiver: ::ethers::core::types::Address,
    }
    #[derive(
        Clone,
        ::ethers::contract::EthEvent,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethevent(
        name = "NewSweeperCommitted",
        abi = "NewSweeperCommitted(address,address,uint256)"
    )]
    pub struct NewSweeperCommittedFilter {
        pub sweeper: ::ethers::core::types::Address,
        pub new_sweeper: ::ethers::core::types::Address,
        pub finalize_time: ::ethers::core::types::U256,
    }
    #[derive(
        Clone,
        ::ethers::contract::EthEvent,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethevent(name = "NewSweeperSet", abi = "NewSweeperSet(address,address)")]
    pub struct NewSweeperSetFilter {
        pub old_sweeper: ::ethers::core::types::Address,
        pub sweeper: ::ethers::core::types::Address,
    }
    #[derive(
        Clone,
        ::ethers::contract::EthEvent,
        ::ethers::contract::EthDisplay,
        Default,
        Debug,
        PartialEq,
        Eq,
        Hash
    )]
    #[ethevent(
        name = "PaymentReceived",
        abi = "PaymentReceived(address,address,uint256)"
    )]
    pub struct PaymentReceivedFilter {
        pub account: ::ethers::core::types::Address,
        pub token: ::ethers::core::types::Address,
        pub amount: ::ethers::core::types::U256,
    }
    ///Container type for all of the contract's events
    #[derive(Clone, ::ethers::contract::EthAbiType, Debug, PartialEq, Eq, Hash)]
    pub enum PaymentFactoryEvents {
        NewOwnerAcceptedFilter(NewOwnerAcceptedFilter),
        NewOwnerCommittedFilter(NewOwnerCommittedFilter),
        NewPaymentAddressFilter(NewPaymentAddressFilter),
        NewReceiverCommittedFilter(NewReceiverCommittedFilter),
        NewReceiverSetFilter(NewReceiverSetFilter),
        NewSweeperCommittedFilter(NewSweeperCommittedFilter),
        NewSweeperSetFilter(NewSweeperSetFilter),
        PaymentReceivedFilter(PaymentReceivedFilter),
    }
    impl ::ethers::contract::EthLogDecode for PaymentFactoryEvents {
        fn decode_log(
            log: &::ethers::core::abi::RawLog,
        ) -> ::core::result::Result<Self, ::ethers::core::abi::Error> {
            if let Ok(decoded) = NewOwnerAcceptedFilter::decode_log(log) {
                return Ok(PaymentFactoryEvents::NewOwnerAcceptedFilter(decoded));
            }
            if let Ok(decoded) = NewOwnerCommittedFilter::decode_log(log) {
                return Ok(PaymentFactoryEvents::NewOwnerCommittedFilter(decoded));
            }
            if let Ok(decoded) = NewPaymentAddressFilter::decode_log(log) {
                return Ok(PaymentFactoryEvents::NewPaymentAddressFilter(decoded));
            }
            if let Ok(decoded) = NewReceiverCommittedFilter::decode_log(log) {
                return Ok(PaymentFactoryEvents::NewReceiverCommittedFilter(decoded));
            }
            if let Ok(decoded) = NewReceiverSetFilter::decode_log(log) {
                return Ok(PaymentFactoryEvents::NewReceiverSetFilter(decoded));
            }
            if let Ok(decoded) = NewSweeperCommittedFilter::decode_log(log) {
                return Ok(PaymentFactoryEvents::NewSweeperCommittedFilter(decoded));
            }
            if let Ok(decoded) = NewSweeperSetFilter::decode_log(log) {
                return Ok(PaymentFactoryEvents::NewSweeperSetFilter(decoded));
            }
            if let Ok(decoded) = PaymentReceivedFilter::decode_log(log) {
                return Ok(PaymentFactoryEvents::PaymentReceivedFilter(decoded));
            }
            Err(::ethers::core::abi::Error::InvalidData)
        }
    }
    impl ::core::fmt::Display for PaymentFactoryEvents {
        fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
            match self {
                Self::NewOwnerAcceptedFilter(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::NewOwnerCommittedFilter(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::NewPaymentAddressFilter(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::NewReceiverCommittedFilter(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::NewReceiverSetFilter(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::NewSweeperCommittedFilter(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::NewSweeperSetFilter(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::PaymentReceivedFilter(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
            }
        }
    }
    impl ::core::convert::From<NewOwnerAcceptedFilter> for PaymentFactoryEvents {
        fn from(value: NewOwnerAcceptedFilter) -> Self {
            Self::NewOwnerAcceptedFilter(value)
        }
    }
    impl ::core::convert::From<NewOwnerCommittedFilter> for PaymentFactoryEvents {
        fn from(value: NewOwnerCommittedFilter) -> Self {
            Self::NewOwnerCommittedFilter(value)
        }
    }
    impl ::core::convert::From<NewPaymentAddressFilter> for PaymentFactoryEvents {
        fn from(value: NewPaymentAddressFilter) -> Self {
            Self::NewPaymentAddressFilter(value)
        }
    }
    impl ::core::convert::From<NewReceiverCommittedFilter> for PaymentFactoryEvents {
        fn from(value: NewReceiverCommittedFilter) -> Self {
            Self::NewReceiverCommittedFilter(value)
        }
    }
    impl ::core::convert::From<NewReceiverSetFilter> for PaymentFactoryEvents {
        fn from(value: NewReceiverSetFilter) -> Self {
            Self::NewReceiverSetFilter(value)
        }
    }
    impl ::core::convert::From<NewSweeperCommittedFilter> for PaymentFactoryEvents {
        fn from(value: NewSweeperCommittedFilter) -> Self {
            Self::NewSweeperCommittedFilter(value)
        }
    }
    impl ::core::convert::From<NewSweeperSetFilter> for PaymentFactoryEvents {
        fn from(value: NewSweeperSetFilter) -> Self {
            Self::NewSweeperSetFilter(value)
        }
    }
    impl ::core::convert::From<PaymentReceivedFilter> for PaymentFactoryEvents {
        fn from(value: PaymentReceivedFilter) -> Self {
            Self::PaymentReceivedFilter(value)
        }
    }
    ///Container type for all input parameters for the `PROXY_IMPLEMENTATION` function with signature `PROXY_IMPLEMENTATION()` and selector `0x5c7a7d99`
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
    #[ethcall(name = "PROXY_IMPLEMENTATION", abi = "PROXY_IMPLEMENTATION()")]
    pub struct ProxyImplementationCall;
    ///Container type for all input parameters for the `accept_transfer_ownership` function with signature `accept_transfer_ownership()` and selector `0xe5ea47b8`
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
    #[ethcall(name = "accept_transfer_ownership", abi = "accept_transfer_ownership()")]
    pub struct AcceptTransferOwnershipCall;
    ///Container type for all input parameters for the `account_to_payment_address` function with signature `account_to_payment_address(address)` and selector `0x0b810230`
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
    #[ethcall(
        name = "account_to_payment_address",
        abi = "account_to_payment_address(address)"
    )]
    pub struct AccountToPaymentAddressCall {
        pub arg_0: ::ethers::core::types::Address,
    }
    ///Container type for all input parameters for the `commit_new_receiver` function with signature `commit_new_receiver(address)` and selector `0x561a4147`
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
    #[ethcall(name = "commit_new_receiver", abi = "commit_new_receiver(address)")]
    pub struct CommitNewReceiverCall {
        pub receiver: ::ethers::core::types::Address,
    }
    ///Container type for all input parameters for the `commit_new_sweeper_implementation` function with signature `commit_new_sweeper_implementation(address)` and selector `0xafcbdc11`
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
    #[ethcall(
        name = "commit_new_sweeper_implementation",
        abi = "commit_new_sweeper_implementation(address)"
    )]
    pub struct CommitNewSweeperImplementationCall {
        pub sweeper: ::ethers::core::types::Address,
    }
    ///Container type for all input parameters for the `commit_transfer_ownership` function with signature `commit_transfer_ownership(address)` and selector `0x6b441a40`
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
    #[ethcall(
        name = "commit_transfer_ownership",
        abi = "commit_transfer_ownership(address)"
    )]
    pub struct CommitTransferOwnershipCall {
        pub new_owner: ::ethers::core::types::Address,
    }
    ///Container type for all input parameters for the `create_payment_address` function with signature `create_payment_address()` and selector `0x6248d5c6`
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
    #[ethcall(name = "create_payment_address", abi = "create_payment_address()")]
    pub struct CreatePaymentAddressCall;
    ///Container type for all input parameters for the `create_payment_address` function with signature `create_payment_address(address)` and selector `0x998faa1e`
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
    #[ethcall(name = "create_payment_address", abi = "create_payment_address(address)")]
    pub struct CreatePaymentAddressWithAccountCall {
        pub account: ::ethers::core::types::Address,
    }
    ///Container type for all input parameters for the `finalize_new_receiver` function with signature `finalize_new_receiver()` and selector `0x73d02b33`
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
    #[ethcall(name = "finalize_new_receiver", abi = "finalize_new_receiver()")]
    pub struct FinalizeNewReceiverCall;
    ///Container type for all input parameters for the `finalize_new_sweeper_implementation` function with signature `finalize_new_sweeper_implementation()` and selector `0x41acefbb`
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
    #[ethcall(
        name = "finalize_new_sweeper_implementation",
        abi = "finalize_new_sweeper_implementation()"
    )]
    pub struct FinalizeNewSweeperImplementationCall;
    ///Container type for all input parameters for the `future_owner` function with signature `future_owner()` and selector `0x1ec0cdc1`
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
    #[ethcall(name = "future_owner", abi = "future_owner()")]
    pub struct FutureOwnerCall;
    ///Container type for all input parameters for the `future_receiver` function with signature `future_receiver()` and selector `0x3bea9ddd`
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
    #[ethcall(name = "future_receiver", abi = "future_receiver()")]
    pub struct FutureReceiverCall;
    ///Container type for all input parameters for the `future_sweeper_implementation` function with signature `future_sweeper_implementation()` and selector `0x4333d02b`
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
    #[ethcall(
        name = "future_sweeper_implementation",
        abi = "future_sweeper_implementation()"
    )]
    pub struct FutureSweeperImplementationCall;
    ///Container type for all input parameters for the `get_approved_tokens` function with signature `get_approved_tokens()` and selector `0x79aaf078`
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
    #[ethcall(name = "get_approved_tokens", abi = "get_approved_tokens()")]
    pub struct GetApprovedTokensCall;
    ///Container type for all input parameters for the `is_approved_token` function with signature `is_approved_token(address)` and selector `0x363b5b3e`
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
    #[ethcall(name = "is_approved_token", abi = "is_approved_token(address)")]
    pub struct IsApprovedTokenCall {
        pub arg_0: ::ethers::core::types::Address,
    }
    ///Container type for all input parameters for the `new_receiver_timestamp` function with signature `new_receiver_timestamp()` and selector `0xf85c2e83`
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
    #[ethcall(name = "new_receiver_timestamp", abi = "new_receiver_timestamp()")]
    pub struct NewReceiverTimestampCall;
    ///Container type for all input parameters for the `new_sweeper_timestamp` function with signature `new_sweeper_timestamp()` and selector `0x05daefa8`
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
    #[ethcall(name = "new_sweeper_timestamp", abi = "new_sweeper_timestamp()")]
    pub struct NewSweeperTimestampCall;
    ///Container type for all input parameters for the `owner` function with signature `owner()` and selector `0x8da5cb5b`
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
    #[ethcall(name = "owner", abi = "owner()")]
    pub struct OwnerCall;
    ///Container type for all input parameters for the `payment_address_to_account` function with signature `payment_address_to_account(address)` and selector `0xe0934982`
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
    #[ethcall(
        name = "payment_address_to_account",
        abi = "payment_address_to_account(address)"
    )]
    pub struct PaymentAddressToAccountCall {
        pub arg_0: ::ethers::core::types::Address,
    }
    ///Container type for all input parameters for the `payment_received` function with signature `payment_received(address,uint256)` and selector `0x7414b098`
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
    #[ethcall(name = "payment_received", abi = "payment_received(address,uint256)")]
    pub struct PaymentReceivedCall {
        pub token: ::ethers::core::types::Address,
        pub amount: ::ethers::core::types::U256,
    }
    ///Container type for all input parameters for the `receiver` function with signature `receiver()` and selector `0xf7260d3e`
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
    #[ethcall(name = "receiver", abi = "receiver()")]
    pub struct ReceiverCall;
    ///Container type for all input parameters for the `set_token_approvals` function with signature `set_token_approvals(address[],bool)` and selector `0x50054612`
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
    #[ethcall(name = "set_token_approvals", abi = "set_token_approvals(address[],bool)")]
    pub struct SetTokenApprovalsCall {
        pub tokens: ::std::vec::Vec<::ethers::core::types::Address>,
        pub approved: bool,
    }
    ///Container type for all input parameters for the `sweeper_implementation` function with signature `sweeper_implementation()` and selector `0x791cc746`
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
    #[ethcall(name = "sweeper_implementation", abi = "sweeper_implementation()")]
    pub struct SweeperImplementationCall;
    ///Container type for all input parameters for the `transfer_ownership_timestamp` function with signature `transfer_ownership_timestamp()` and selector `0x4598cb25`
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
    #[ethcall(
        name = "transfer_ownership_timestamp",
        abi = "transfer_ownership_timestamp()"
    )]
    pub struct TransferOwnershipTimestampCall;
    ///Container type for all of the contract's call
    #[derive(Clone, ::ethers::contract::EthAbiType, Debug, PartialEq, Eq, Hash)]
    pub enum PaymentFactoryCalls {
        ProxyImplementation(ProxyImplementationCall),
        AcceptTransferOwnership(AcceptTransferOwnershipCall),
        AccountToPaymentAddress(AccountToPaymentAddressCall),
        CommitNewReceiver(CommitNewReceiverCall),
        CommitNewSweeperImplementation(CommitNewSweeperImplementationCall),
        CommitTransferOwnership(CommitTransferOwnershipCall),
        CreatePaymentAddress(CreatePaymentAddressCall),
        CreatePaymentAddressWithAccount(CreatePaymentAddressWithAccountCall),
        FinalizeNewReceiver(FinalizeNewReceiverCall),
        FinalizeNewSweeperImplementation(FinalizeNewSweeperImplementationCall),
        FutureOwner(FutureOwnerCall),
        FutureReceiver(FutureReceiverCall),
        FutureSweeperImplementation(FutureSweeperImplementationCall),
        GetApprovedTokens(GetApprovedTokensCall),
        IsApprovedToken(IsApprovedTokenCall),
        NewReceiverTimestamp(NewReceiverTimestampCall),
        NewSweeperTimestamp(NewSweeperTimestampCall),
        Owner(OwnerCall),
        PaymentAddressToAccount(PaymentAddressToAccountCall),
        PaymentReceived(PaymentReceivedCall),
        Receiver(ReceiverCall),
        SetTokenApprovals(SetTokenApprovalsCall),
        SweeperImplementation(SweeperImplementationCall),
        TransferOwnershipTimestamp(TransferOwnershipTimestampCall),
    }
    impl ::ethers::core::abi::AbiDecode for PaymentFactoryCalls {
        fn decode(
            data: impl AsRef<[u8]>,
        ) -> ::core::result::Result<Self, ::ethers::core::abi::AbiError> {
            let data = data.as_ref();
            if let Ok(decoded)
                = <ProxyImplementationCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::ProxyImplementation(decoded));
            }
            if let Ok(decoded)
                = <AcceptTransferOwnershipCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::AcceptTransferOwnership(decoded));
            }
            if let Ok(decoded)
                = <AccountToPaymentAddressCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::AccountToPaymentAddress(decoded));
            }
            if let Ok(decoded)
                = <CommitNewReceiverCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::CommitNewReceiver(decoded));
            }
            if let Ok(decoded)
                = <CommitNewSweeperImplementationCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::CommitNewSweeperImplementation(decoded));
            }
            if let Ok(decoded)
                = <CommitTransferOwnershipCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::CommitTransferOwnership(decoded));
            }
            if let Ok(decoded)
                = <CreatePaymentAddressCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::CreatePaymentAddress(decoded));
            }
            if let Ok(decoded)
                = <CreatePaymentAddressWithAccountCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::CreatePaymentAddressWithAccount(decoded));
            }
            if let Ok(decoded)
                = <FinalizeNewReceiverCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::FinalizeNewReceiver(decoded));
            }
            if let Ok(decoded)
                = <FinalizeNewSweeperImplementationCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::FinalizeNewSweeperImplementation(decoded));
            }
            if let Ok(decoded)
                = <FutureOwnerCall as ::ethers::core::abi::AbiDecode>::decode(data) {
                return Ok(Self::FutureOwner(decoded));
            }
            if let Ok(decoded)
                = <FutureReceiverCall as ::ethers::core::abi::AbiDecode>::decode(data) {
                return Ok(Self::FutureReceiver(decoded));
            }
            if let Ok(decoded)
                = <FutureSweeperImplementationCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::FutureSweeperImplementation(decoded));
            }
            if let Ok(decoded)
                = <GetApprovedTokensCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::GetApprovedTokens(decoded));
            }
            if let Ok(decoded)
                = <IsApprovedTokenCall as ::ethers::core::abi::AbiDecode>::decode(data) {
                return Ok(Self::IsApprovedToken(decoded));
            }
            if let Ok(decoded)
                = <NewReceiverTimestampCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::NewReceiverTimestamp(decoded));
            }
            if let Ok(decoded)
                = <NewSweeperTimestampCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::NewSweeperTimestamp(decoded));
            }
            if let Ok(decoded)
                = <OwnerCall as ::ethers::core::abi::AbiDecode>::decode(data) {
                return Ok(Self::Owner(decoded));
            }
            if let Ok(decoded)
                = <PaymentAddressToAccountCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::PaymentAddressToAccount(decoded));
            }
            if let Ok(decoded)
                = <PaymentReceivedCall as ::ethers::core::abi::AbiDecode>::decode(data) {
                return Ok(Self::PaymentReceived(decoded));
            }
            if let Ok(decoded)
                = <ReceiverCall as ::ethers::core::abi::AbiDecode>::decode(data) {
                return Ok(Self::Receiver(decoded));
            }
            if let Ok(decoded)
                = <SetTokenApprovalsCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::SetTokenApprovals(decoded));
            }
            if let Ok(decoded)
                = <SweeperImplementationCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::SweeperImplementation(decoded));
            }
            if let Ok(decoded)
                = <TransferOwnershipTimestampCall as ::ethers::core::abi::AbiDecode>::decode(
                    data,
                ) {
                return Ok(Self::TransferOwnershipTimestamp(decoded));
            }
            Err(::ethers::core::abi::Error::InvalidData.into())
        }
    }
    impl ::ethers::core::abi::AbiEncode for PaymentFactoryCalls {
        fn encode(self) -> Vec<u8> {
            match self {
                Self::ProxyImplementation(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::AcceptTransferOwnership(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::AccountToPaymentAddress(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::CommitNewReceiver(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::CommitNewSweeperImplementation(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::CommitTransferOwnership(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::CreatePaymentAddress(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::CreatePaymentAddressWithAccount(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::FinalizeNewReceiver(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::FinalizeNewSweeperImplementation(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::FutureOwner(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::FutureReceiver(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::FutureSweeperImplementation(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::GetApprovedTokens(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::IsApprovedToken(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::NewReceiverTimestamp(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::NewSweeperTimestamp(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::Owner(element) => ::ethers::core::abi::AbiEncode::encode(element),
                Self::PaymentAddressToAccount(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::PaymentReceived(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::Receiver(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::SetTokenApprovals(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::SweeperImplementation(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
                Self::TransferOwnershipTimestamp(element) => {
                    ::ethers::core::abi::AbiEncode::encode(element)
                }
            }
        }
    }
    impl ::core::fmt::Display for PaymentFactoryCalls {
        fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
            match self {
                Self::ProxyImplementation(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::AcceptTransferOwnership(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::AccountToPaymentAddress(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::CommitNewReceiver(element) => ::core::fmt::Display::fmt(element, f),
                Self::CommitNewSweeperImplementation(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::CommitTransferOwnership(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::CreatePaymentAddress(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::CreatePaymentAddressWithAccount(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::FinalizeNewReceiver(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::FinalizeNewSweeperImplementation(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::FutureOwner(element) => ::core::fmt::Display::fmt(element, f),
                Self::FutureReceiver(element) => ::core::fmt::Display::fmt(element, f),
                Self::FutureSweeperImplementation(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::GetApprovedTokens(element) => ::core::fmt::Display::fmt(element, f),
                Self::IsApprovedToken(element) => ::core::fmt::Display::fmt(element, f),
                Self::NewReceiverTimestamp(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::NewSweeperTimestamp(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::Owner(element) => ::core::fmt::Display::fmt(element, f),
                Self::PaymentAddressToAccount(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::PaymentReceived(element) => ::core::fmt::Display::fmt(element, f),
                Self::Receiver(element) => ::core::fmt::Display::fmt(element, f),
                Self::SetTokenApprovals(element) => ::core::fmt::Display::fmt(element, f),
                Self::SweeperImplementation(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
                Self::TransferOwnershipTimestamp(element) => {
                    ::core::fmt::Display::fmt(element, f)
                }
            }
        }
    }
    impl ::core::convert::From<ProxyImplementationCall> for PaymentFactoryCalls {
        fn from(value: ProxyImplementationCall) -> Self {
            Self::ProxyImplementation(value)
        }
    }
    impl ::core::convert::From<AcceptTransferOwnershipCall> for PaymentFactoryCalls {
        fn from(value: AcceptTransferOwnershipCall) -> Self {
            Self::AcceptTransferOwnership(value)
        }
    }
    impl ::core::convert::From<AccountToPaymentAddressCall> for PaymentFactoryCalls {
        fn from(value: AccountToPaymentAddressCall) -> Self {
            Self::AccountToPaymentAddress(value)
        }
    }
    impl ::core::convert::From<CommitNewReceiverCall> for PaymentFactoryCalls {
        fn from(value: CommitNewReceiverCall) -> Self {
            Self::CommitNewReceiver(value)
        }
    }
    impl ::core::convert::From<CommitNewSweeperImplementationCall>
    for PaymentFactoryCalls {
        fn from(value: CommitNewSweeperImplementationCall) -> Self {
            Self::CommitNewSweeperImplementation(value)
        }
    }
    impl ::core::convert::From<CommitTransferOwnershipCall> for PaymentFactoryCalls {
        fn from(value: CommitTransferOwnershipCall) -> Self {
            Self::CommitTransferOwnership(value)
        }
    }
    impl ::core::convert::From<CreatePaymentAddressCall> for PaymentFactoryCalls {
        fn from(value: CreatePaymentAddressCall) -> Self {
            Self::CreatePaymentAddress(value)
        }
    }
    impl ::core::convert::From<CreatePaymentAddressWithAccountCall>
    for PaymentFactoryCalls {
        fn from(value: CreatePaymentAddressWithAccountCall) -> Self {
            Self::CreatePaymentAddressWithAccount(value)
        }
    }
    impl ::core::convert::From<FinalizeNewReceiverCall> for PaymentFactoryCalls {
        fn from(value: FinalizeNewReceiverCall) -> Self {
            Self::FinalizeNewReceiver(value)
        }
    }
    impl ::core::convert::From<FinalizeNewSweeperImplementationCall>
    for PaymentFactoryCalls {
        fn from(value: FinalizeNewSweeperImplementationCall) -> Self {
            Self::FinalizeNewSweeperImplementation(value)
        }
    }
    impl ::core::convert::From<FutureOwnerCall> for PaymentFactoryCalls {
        fn from(value: FutureOwnerCall) -> Self {
            Self::FutureOwner(value)
        }
    }
    impl ::core::convert::From<FutureReceiverCall> for PaymentFactoryCalls {
        fn from(value: FutureReceiverCall) -> Self {
            Self::FutureReceiver(value)
        }
    }
    impl ::core::convert::From<FutureSweeperImplementationCall> for PaymentFactoryCalls {
        fn from(value: FutureSweeperImplementationCall) -> Self {
            Self::FutureSweeperImplementation(value)
        }
    }
    impl ::core::convert::From<GetApprovedTokensCall> for PaymentFactoryCalls {
        fn from(value: GetApprovedTokensCall) -> Self {
            Self::GetApprovedTokens(value)
        }
    }
    impl ::core::convert::From<IsApprovedTokenCall> for PaymentFactoryCalls {
        fn from(value: IsApprovedTokenCall) -> Self {
            Self::IsApprovedToken(value)
        }
    }
    impl ::core::convert::From<NewReceiverTimestampCall> for PaymentFactoryCalls {
        fn from(value: NewReceiverTimestampCall) -> Self {
            Self::NewReceiverTimestamp(value)
        }
    }
    impl ::core::convert::From<NewSweeperTimestampCall> for PaymentFactoryCalls {
        fn from(value: NewSweeperTimestampCall) -> Self {
            Self::NewSweeperTimestamp(value)
        }
    }
    impl ::core::convert::From<OwnerCall> for PaymentFactoryCalls {
        fn from(value: OwnerCall) -> Self {
            Self::Owner(value)
        }
    }
    impl ::core::convert::From<PaymentAddressToAccountCall> for PaymentFactoryCalls {
        fn from(value: PaymentAddressToAccountCall) -> Self {
            Self::PaymentAddressToAccount(value)
        }
    }
    impl ::core::convert::From<PaymentReceivedCall> for PaymentFactoryCalls {
        fn from(value: PaymentReceivedCall) -> Self {
            Self::PaymentReceived(value)
        }
    }
    impl ::core::convert::From<ReceiverCall> for PaymentFactoryCalls {
        fn from(value: ReceiverCall) -> Self {
            Self::Receiver(value)
        }
    }
    impl ::core::convert::From<SetTokenApprovalsCall> for PaymentFactoryCalls {
        fn from(value: SetTokenApprovalsCall) -> Self {
            Self::SetTokenApprovals(value)
        }
    }
    impl ::core::convert::From<SweeperImplementationCall> for PaymentFactoryCalls {
        fn from(value: SweeperImplementationCall) -> Self {
            Self::SweeperImplementation(value)
        }
    }
    impl ::core::convert::From<TransferOwnershipTimestampCall> for PaymentFactoryCalls {
        fn from(value: TransferOwnershipTimestampCall) -> Self {
            Self::TransferOwnershipTimestamp(value)
        }
    }
    ///Container type for all return fields from the `PROXY_IMPLEMENTATION` function with signature `PROXY_IMPLEMENTATION()` and selector `0x5c7a7d99`
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
    pub struct ProxyImplementationReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `account_to_payment_address` function with signature `account_to_payment_address(address)` and selector `0x0b810230`
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
    pub struct AccountToPaymentAddressReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `future_owner` function with signature `future_owner()` and selector `0x1ec0cdc1`
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
    pub struct FutureOwnerReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `future_receiver` function with signature `future_receiver()` and selector `0x3bea9ddd`
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
    pub struct FutureReceiverReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `future_sweeper_implementation` function with signature `future_sweeper_implementation()` and selector `0x4333d02b`
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
    pub struct FutureSweeperImplementationReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `get_approved_tokens` function with signature `get_approved_tokens()` and selector `0x79aaf078`
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
    pub struct GetApprovedTokensReturn(
        pub ::std::vec::Vec<::ethers::core::types::Address>,
    );
    ///Container type for all return fields from the `is_approved_token` function with signature `is_approved_token(address)` and selector `0x363b5b3e`
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
    pub struct IsApprovedTokenReturn(pub bool);
    ///Container type for all return fields from the `new_receiver_timestamp` function with signature `new_receiver_timestamp()` and selector `0xf85c2e83`
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
    pub struct NewReceiverTimestampReturn(pub ::ethers::core::types::U256);
    ///Container type for all return fields from the `new_sweeper_timestamp` function with signature `new_sweeper_timestamp()` and selector `0x05daefa8`
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
    pub struct NewSweeperTimestampReturn(pub ::ethers::core::types::U256);
    ///Container type for all return fields from the `owner` function with signature `owner()` and selector `0x8da5cb5b`
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
    pub struct OwnerReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `payment_address_to_account` function with signature `payment_address_to_account(address)` and selector `0xe0934982`
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
    pub struct PaymentAddressToAccountReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `payment_received` function with signature `payment_received(address,uint256)` and selector `0x7414b098`
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
    pub struct PaymentReceivedReturn(pub bool);
    ///Container type for all return fields from the `receiver` function with signature `receiver()` and selector `0xf7260d3e`
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
    pub struct ReceiverReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `sweeper_implementation` function with signature `sweeper_implementation()` and selector `0x791cc746`
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
    pub struct SweeperImplementationReturn(pub ::ethers::core::types::Address);
    ///Container type for all return fields from the `transfer_ownership_timestamp` function with signature `transfer_ownership_timestamp()` and selector `0x4598cb25`
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
    pub struct TransferOwnershipTimestampReturn(pub ::ethers::core::types::U256);
}
