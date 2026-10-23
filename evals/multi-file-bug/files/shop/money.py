def to_cents(amount):
    """An amount in dollars as whole cents."""
    return int(amount * 100)


def fmt(cents):
    return f"${cents // 100}.{cents % 100:02d}"
