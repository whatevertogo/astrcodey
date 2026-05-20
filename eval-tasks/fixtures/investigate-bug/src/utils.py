"""Utility functions used by the main module."""

def parse_csv_line(line):
    """Parse a CSV line respecting quoted fields."""
    fields = []
    current = ""
    in_quotes = False
    for char in line:
        if char == '"':
            in_quotes = not in_quotes
        elif char == ',' and not in_quotes:
            fields.append(current.strip())
            current = ""
        else:
            current += char
    fields.append(current.strip())
    return fields


def validate_email(email):
    """Basic email validation."""
    # Bug: doesn't handle + addresses correctly
    if '@' not in email:
        return False
    local, domain = email.rsplit('@', 1)
    if not local or not domain:
        return False
    if '.' not in domain:
        return False
    # Bug: rejects valid + addresses
    if '+' in local:
        return False
    return True


def normalize_phone(phone):
    """Normalize phone number to digits only."""
    return ''.join(c for c in phone if c.isdigit())
