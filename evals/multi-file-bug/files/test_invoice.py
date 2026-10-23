from shop.invoice import Invoice


def test_single_item():
    i = Invoice()
    i.add("gum", 0.29)
    assert i.total_cents() == 29


def test_discount():
    i = Invoice("SAVE10")
    i.add("book", 20.0)
    assert i.total_cents() == 1800


def test_render():
    i = Invoice()
    i.add("pen", 1.15)
    assert i.render() == "total $1.15"
