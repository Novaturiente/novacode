from pkg.cart import Cart
from pkg import money


def test_total_with_tax():
    c = Cart()
    c.prices += [10, 20]
    assert c.total(0.1) == 33.0


def test_new_name():
    assert money.total_with_tax([1, 1], 0.5) == 3.0
