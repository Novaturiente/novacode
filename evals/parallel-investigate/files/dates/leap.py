def is_leap(year):
    """Gregorian leap year."""
    return year % 4 == 0 and year % 100 != 0
