The `shop` MCP server knows the orders of customer `C-4471`. MCP tools are reached through codemode.

1. Find the customer's shipped orders.
2. Get their totals and add them up, in cents.
3. Look at the receipt image of the largest shipped order and name its colour.

Write the result to `answer.json` in this folder, exactly this shape:

```json
{"total_cents": 1234, "receipt_colour": "red"}
```

`receipt_colour` is one plain lower-case colour name. Don't guess any value: get each one from the server.
