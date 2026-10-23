python3 run_tests.py >/dev/null && git diff --quiet -- test_geo.py &&
[ "$(python3 report.py)" = "route 12.00, nearest (1, 1)" ] &&
python3 -c '
import inspect
from geo.dist import distance
assert len(inspect.signature(distance).parameters) == 2
assert distance((1, 1), (4, 5)) == 5.0
'
