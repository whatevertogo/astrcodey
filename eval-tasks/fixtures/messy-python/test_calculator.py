"""Tests that must pass after refactoring."""
import subprocess
import sys
import tempfile
import os

def run_calculator(input_content):
    """Run calculator.py with given input and return stdout."""
    with tempfile.NamedTemporaryFile(mode='w', suffix='.csv', delete=False) as f:
        f.write(input_content)
        f.flush()
        result = subprocess.run(
            [sys.executable, 'calculator.py', f.name],
            capture_output=True, text=True, cwd=os.path.dirname(os.path.abspath(__file__))
        )
    os.unlink(f.name)
    return result.stdout, result.returncode

def test_basic_operations():
    stdout, code = run_calculator("10,5,add\n10,5,sub\n10,5,mul\n10,2,div\n")
    assert code == 0
    lines = [l for l in stdout.strip().split('\n') if l.startswith('Result')]
    assert '15' in lines[0]
    assert '5' in lines[1]
    assert '50' in lines[2]
    assert '5' in lines[3]

def test_division_by_zero():
    stdout, code = run_calculator("10,0,div\n")
    assert code == 0
    assert 'ERROR' in stdout

def test_power_and_mod():
    stdout, code = run_calculator("2,10,pow\n10,3,mod\n")
    assert code == 0
    lines = [l for l in stdout.strip().split('\n') if l.startswith('Result')]
    assert '1024' in lines[0]
    assert '1' in lines[1]

if __name__ == '__main__':
    test_basic_operations()
    test_division_by_zero()
    test_power_and_mod()
    print("All tests passed!")
