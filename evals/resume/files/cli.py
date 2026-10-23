import sys

from text import shout

if __name__ == "__main__":
    print(shout(" ".join(sys.argv[1:])))
