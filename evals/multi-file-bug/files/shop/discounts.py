CODES = {"SAVE10": 10, "HALF": 50}


def apply(amount, code=None):
    """The amount after a percentage discount code (unknown codes give no discount)."""
    pct = CODES.get(code, 0)
    return amount * (100 - pct) / 100
