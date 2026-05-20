"""Data processor — reads CSV, validates, and outputs clean data."""
from .utils import parse_csv_line, validate_email, normalize_phone


def process_contacts(input_path, output_path):
    """Process contacts CSV: validate emails, normalize phones, write clean output."""
    valid_count = 0
    invalid_count = 0

    with open(input_path) as fin, open(output_path, 'w') as fout:
        header = fin.readline().strip()
        fout.write(header + '\n')

        for line in fin:
            line = line.strip()
            if not line:
                continue
            fields = parse_csv_line(line)
            if len(fields) < 3:
                invalid_count += 1
                continue

            name, email, phone = fields[0], fields[1], fields[2]

            if not validate_email(email):
                invalid_count += 1
                continue

            phone = normalize_phone(phone)
            fout.write(f"{name},{email},{phone}\n")
            valid_count += 1

    return valid_count, invalid_count
