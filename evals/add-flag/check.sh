[ "$(python3 wc.py a.txt b.txt)" = "$(printf '4 a.txt\n1 b.txt\n5 total')" ] &&
[ "$(python3 wc.py -l a.txt b.txt)" = "$(printf '2 a.txt\n1 b.txt\n3 total')" ]
