from brownie import Contract, Sweeper, accounts
from brownie.network import priority_fee

def main():
    print("Hello")


    print("accounts are")
    token = Contract.from_explorer("0xC9fCFA7e28fF320C49967f4522EBc709aa1fDE7c")
    factory = Contract.from_explorer("0x4e3bc2054788de923a04936c6addb99a05b0ea36")
    user = accounts.load("david")
    # user = accounts.load("david-main")

    print("Llama token")
    print(token)

    print("Factory token")
    print(factory)

    print("User addr")
    print(user)

    # Sweeper and Proxy are deployed by us, as the user, by calling factory
    # Already been called before ...
    # factory.create_payment_address({'from': user})
    sweeper = Sweeper.at(factory.account_to_payment_address(user))
    print("Sweeper is at")
    print(sweeper)

    priority_fee("auto")
    token._mint_for_testing(user, (10_000)*(10**18), {'from': user})
    # token.approve(sweeper, 2**256-1, {'from': user})
    sweeper.send_token(token, (5_000)*(10**18), {'from': user})
    # sweeper.send_token(token, (47)*(10**13), {'from': user})
