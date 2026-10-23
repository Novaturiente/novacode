from geo.dist import distance
from geo.nearest import nearest
from geo.route import length


def test_distance_takes_points():
    assert distance((0, 0), (3, 4)) == 5.0


def test_route_length():
    assert length([(0, 0), (3, 4), (3, 0)]) == 9.0


def test_nearest():
    assert nearest((10, 10), [(0, 0), (9, 8), (20, 20)]) == (9, 8)
