from geo.dist import distance


def length(points):
    """Total length of a path through the points in order."""
    total = 0.0
    for (x1, y1), (x2, y2) in zip(points, points[1:]):
        total += distance(x1, y1, x2, y2)
    return total
