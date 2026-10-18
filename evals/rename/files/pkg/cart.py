from pkg.money import calc_tot


class Cart:
    def __init__(self):
        self.prices = []

    def total(self, tax=0.0):
        return calc_tot(self.prices, tax)
