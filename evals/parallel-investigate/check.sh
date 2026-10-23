python3 run_tests.py >/dev/null && git diff --quiet -- test_all.py &&
python3 -c '
from stats.mean import mean
from strings.pad import left_pad
from dates.leap import is_leap
from units.temp import c_to_f
assert mean([]) == 0.0 and mean([2, 4]) == 3.0
assert left_pad("ab", 5, ".") == "...ab" and left_pad("abcdef", 3) == "abcdef"
assert [is_leap(y) for y in (1600, 1700, 2000, 2023, 2024, 2100)] == [True, False, True, False, True, False]
assert c_to_f(-40) == -40 and abs(c_to_f(37) - 98.6) < 1e-9
assert c_to_f(100) == 212
'
