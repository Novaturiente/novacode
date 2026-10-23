from geo import dist


def nearest(origin, points):
    """The point closest to origin."""
    ox, oy = origin
    return min(points, key=lambda p: dist.distance(ox, oy, p[0], p[1]))
