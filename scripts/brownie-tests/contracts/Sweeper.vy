# @version 0.3.7

from vyper.interfaces import ERC20

interface Factory:
    def payment_received(token: ERC20, amount: uint256) -> bool: nonpayable
    def payment_address_to_account(account: address) -> address: view
    def owner() -> address: view
    def receiver() -> address: view


FACTORY: public(immutable(Factory))


@external
def __init__(_factory: Factory):
    FACTORY = _factory


@external
def send_token(_token: ERC20, _amount: uint256):
    assert _amount > 0
    _token.transferFrom(msg.sender, self, _amount)
    receiver: address = FACTORY.receiver()
    _token.transfer(receiver, _amount)

    FACTORY.payment_received(_token, _amount)


@external
def sweep_token_balance(_token: ERC20):
    receiver: address = FACTORY.receiver()
    amount: uint256 = _token.balanceOf(self)
    if amount > 0:
        _token.transfer(receiver, amount)
        FACTORY.payment_received(_token, amount)


@external
def recover_token_balance(_token: ERC20):
    assert msg.sender == FACTORY.payment_address_to_account(self) or msg.sender == FACTORY.owner()
    amount: uint256 = _token.balanceOf(self)
    _token.transfer(msg.sender, amount)
