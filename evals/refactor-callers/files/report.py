from geo.dist import distance
from geo.nearest import nearest

ROUTE = [(0, 0), (3, 4), (6, 8), (6, 10)]

if __name__ == "__main__":
    total = sum(distance(a[0], a[1], b[0], b[1]) for a, b in zip(ROUTE, ROUTE[1:]))
    print(f"route {total:.2f}, nearest {nearest((0, 0), [(5, 5), (1, 1), (3, 0)])}")
