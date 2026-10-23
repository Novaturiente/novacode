Four independent modules each have one defect. `python3 run_tests.py` shows them. The modules don't depend on each other, so you may split the investigation across subagents, one per module.

- `stats/mean.py`: `mean([])` raises instead of returning `0.0`.
- `strings/pad.py`: `left_pad` puts the padding on the wrong side.
- `dates/leap.py`: `is_leap` is wrong for some century years.
- `units/temp.py`: `c_to_f` gives wrong results.

Fix all four in their own modules. Don't change the tests.
