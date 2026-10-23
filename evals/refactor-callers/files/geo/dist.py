import math


def distance(x1, y1, x2, y2):
    """Straight-line distance between (x1, y1) and (x2, y2)."""
    return math.hypot(x2 - x1, y2 - y1)
