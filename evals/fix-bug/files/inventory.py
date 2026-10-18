"""A tiny inventory: items with quantities and prices."""


class Inventory:
    def __init__(self):
        self.items = {}

    def add(self, name, qty, price):
        self.items[name] = {"qty": qty, "price": price}

    def remove(self, name, qty):
        self.items[name]["qty"] -= qty

    def total_value(self):
        return sum(i["qty"] * i["price"] for i in self.items.values())
