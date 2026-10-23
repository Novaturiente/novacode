from shop import discounts
from shop.money import fmt, to_cents


class Invoice:
    def __init__(self, code=None):
        self.lines = []
        self.code = code

    def add(self, name, price, qty=1):
        self.lines.append((name, price, qty))

    def total_cents(self):
        subtotal = sum(price * qty for _, price, qty in self.lines)
        return to_cents(discounts.apply(subtotal, self.code))

    def render(self):
        return f"total {fmt(self.total_cents())}"
