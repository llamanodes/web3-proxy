/// Accepted Currencies, their respective ERC20 token function, data and decimals

pub struct Currency {
    name: String,
    address: Address,
    decimals: u8
}

const CURRENCIES: HashMap<Address, Currency> = lazy_static! {
    let mut currencies: HashMap<Address, Currency> = HashMap();

    currencies.insert(
        "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse::<Address>().unwrap(),
        {
            name: "USDT",
            address: "0xdAC17F958D2ee523a2206206994597C13D831ec7".parse::<Address>().unwrap(),
            decimals: 6
        }
    );

    currencies.insert(
        "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse::<Address>().unwrap(),
        {
            name: "USDC",
            address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse::<Address>().unwrap(),
            decimals: 6
        }
    );

    currencies
};
