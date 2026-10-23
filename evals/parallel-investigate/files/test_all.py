from dates.leap import is_leap
from stats.mean import mean
from strings.pad import left_pad
from units.temp import c_to_f


def test_mean_empty():
    assert mean([]) == 0.0


def test_mean():
    assert mean([1, 2, 3]) == 2.0


def test_left_pad():
    assert left_pad("7", 3, "0") == "007"


def test_leap_centuries():
    assert is_leap(2000) and not is_leap(1900)


def test_leap_plain():
    assert is_leap(2024) and not is_leap(2023)


def test_c_to_f():
    assert c_to_f(0) == 32 and c_to_f(100) == 212
