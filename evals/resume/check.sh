python3 -c '
from text import slugify
assert slugify("Hello, World!") == "hello-world"
assert slugify("  --A  b__C 9 ") == "a-b-c-9"
assert slugify("x") == "x"
' &&
[ "$(python3 cli.py --slug 'Hello, World!')" = "hello-world" ] &&
[ "$(python3 cli.py 'Hello, World!')" = "HELLO, WORLD!" ] &&
grep -Eq '^(from text import .*slugify|import text)' cli.py
