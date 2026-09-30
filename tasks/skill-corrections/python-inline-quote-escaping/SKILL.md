---
name: python-inline-quote-escaping
description: Avoid SyntaxError in inline Python one-liners by alternating quote delimiters and avoiding backslashes in f-strings.
---

# Inline Python Quote Escaping

## When to use
- Executing inline Python commands with `python3 -c` in shell environments.
- Constructing f-strings that access dictionary keys, call functions, or format strings.
- Encountering `SyntaxError: unexpected character after line continuation character` or invalid syntax in one-liners.

## Procedure

1. **Avoid Backslashes Inside F-Strings**
   Do not use backslash-escaped quotes inside f-string interpolation expressions (`{...}`). In Python versions prior to 3.12, backslashes are strictly forbidden inside f-string expressions.
   - Incorrect:
     ```python
     f"ID: {data.get(\"id\")}"
     ```
   - Correct (alternate quote types):
     ```python
     f"ID: {data.get('id')}"
     ```

2. **Alternate Quotes in Shell One-Liners**
   When invoking Python from Bash or Zsh, wrap the inline script in single quotes and use double quotes inside Python:
   ```bash
   python3 -c 'data = {"name": "Alice"}; print("name=" + data["name"])'
   ```
   Or wrap the shell command in double quotes and use single quotes inside Python:
   ```bash
   python3 -c "data = {'name': 'Alice'}; print(f'name={data[\"name\"]}')"
   ```

3. **Separate Expression Evaluation into Variables**
   Extract complex dictionary lookups or operations into intermediate variables before string formatting to avoid nested quotes:
   ```bash
   python3 -c 'import json; d = json.loads(val); name = d.get("name", ""); print(f"Name: {name}")'
   ```

4. **Use Argument Unpacking or str.format()**
   When nested quoting is inconvenient, use comma-separated arguments in `print()` or `str.format()`:
   ```bash
   python3 -c 'print("ID:", data.get("id"), "Title:", data.get("title"))'
   ```

5. **Use Multi-Line Heredocs for Multi-Statement Logic**
   For complex scripts requiring multiple regexes or nested quotes, avoid cramming the logic into an inline one-liner flag. Pipe the script via standard input:
   ```bash
   python3 << 'EOF'
   import json
   for item in items:
       print(f"{item['id']}: {item['title']}")
   EOF
   ```
