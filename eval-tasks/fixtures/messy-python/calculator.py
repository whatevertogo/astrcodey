## this is a calculator i wrote in college dont judge me lol
import sys
import os

def calc(a,b,op):
    if op == "add":
        result = a + b
        return result
    elif op == "sub":
        result = a - b
        return result
    elif op == "mul":
        result = a * b
        return result
    elif op == "div":
        if b == 0:
            print("ERROR: cannot divide by zero!!!")
            return None
        else:
            result = a / b
            return result
    elif op == "pow":
        result = a ** b
        return result
    elif op == "mod":
        if b == 0:
            print("ERROR: cannot mod by zero!!!")
            return None
        result = a % b
        return result
    else:
        print("ERROR: unknown operation " + str(op))
        return None

def process_batch(data):
    results = []
    i = 0
    while i < len(data):
        item = data[i]
        a = item[0]
        b = item[1]
        op = item[2]
        r = calc(a, b, op)
        if r is not None:
            results.append(r)
        else:
            results.append("ERROR")
        i = i + 1
    return results

def read_from_file(filename):
    f = open(filename, "r")
    lines = f.readlines()
    f.close()
    data = []
    for line in lines:
        parts = line.strip().split(",")
        if len(parts) == 3:
            try:
                a = float(parts[0])
                b = float(parts[1])
                op = parts[2].strip()
                data.append((a, b, op))
            except:
                pass
    return data

def main():
    if len(sys.argv) < 2:
        print("Usage: python calculator.py <input_file>")
        sys.exit(1)
    filename = sys.argv[1]
    if not os.path.exists(filename):
        print("File not found: " + filename)
        sys.exit(1)
    data = read_from_file(filename)
    results = process_batch(data)
    for i in range(len(results)):
        print("Result " + str(i+1) + ": " + str(results[i]))

if __name__ == "__main__":
    main()
