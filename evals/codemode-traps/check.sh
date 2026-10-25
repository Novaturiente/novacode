# answer.json: total of C-4471's shipped orders (1250 + 4999 + 870) and the colour of
# the receipt of the largest one (ORD-2205, solid blue).
python3 - <<'EOF'
import json, sys
a = json.load(open("answer.json"))
ok = a.get("total_cents") == 7119 and str(a.get("receipt_colour", "")).strip().lower() == "blue"
sys.exit(0 if ok else 1)
EOF
