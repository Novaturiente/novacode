python3 run_tests.py >/dev/null && git diff --quiet -- test_invoice.py &&
python3 -c '
from shop.money import to_cents
assert [to_cents(x) for x in (1.15, 0.07, 0.29, 10, 2.3)] == [115, 7, 29, 1000, 230]
'
