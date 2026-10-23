def left_pad(s, width, fill=" "):
    """s right-aligned in width characters, padded on the left with fill."""
    return s + fill * (width - len(s))
