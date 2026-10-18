from inventory import Inventory


def test_add_twice_accumulates():
    i = Inventory()
    i.add("pen", 2, 1.5)
    i.add("pen", 3, 1.5)
    assert i.items["pen"]["qty"] == 5


def test_total_value():
    i = Inventory()
    i.add("pen", 2, 1.5)
    i.add("cup", 1, 4.0)
    assert i.total_value() == 7.0
