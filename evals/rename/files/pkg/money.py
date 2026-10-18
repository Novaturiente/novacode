def calc_tot(prices, tax):
    """Sum of prices plus tax (a fraction)."""
    return round(sum(prices) * (1 + tax), 2)
