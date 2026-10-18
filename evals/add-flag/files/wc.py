"""Count words in files: python3 wc.py FILE..."""
import sys


def main(argv):
    total = 0
    for path in argv:
        with open(path) as f:
            n = len(f.read().split())
        print(f"{n} {path}")
        total += n
    if len(argv) > 1:
        print(f"{total} total")


if __name__ == "__main__":
    main(sys.argv[1:])
