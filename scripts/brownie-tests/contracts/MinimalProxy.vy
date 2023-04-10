# @version 0.3.7


interface Factory:
    def sweeper_implementation() -> address: view


FACTORY: immutable(Factory)


@external
def __init__(_factory: Factory):
    FACTORY = _factory


@external
def __default__():
    implementation: address = FACTORY.sweeper_implementation()
    raw_call(implementation, msg.data, is_delegate_call=True)


@view
@external
def implementation() -> address:
    return FACTORY.sweeper_implementation()
