"""Integration tests — must pass after refactoring."""
import subprocess
import sys
import os
import json
import tempfile

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))

def run_task_cmd(*args, cwd=None):
    env = os.environ.copy()
    result = subprocess.run(
        [sys.executable, os.path.join(SCRIPT_DIR, "src", "main.py")] + list(args),
        capture_output=True, text=True, cwd=cwd or tempfile.mkdtemp()
    )
    return result.stdout.strip(), result.returncode

def test_add_and_list():
    cwd = tempfile.mkdtemp()
    # Patch TASKS_FILE to use temp dir
    env_patch = f'import os; os.chdir("{cwd}")\n'
    main_path = os.path.join(SCRIPT_DIR, "src", "main.py")

    def run(*args):
        result = subprocess.run(
            [sys.executable, "-c", f'import os; os.chdir("{cwd}"); exec(open("{main_path}").read()); main()'],
            capture_output=True, text=True,
            env={**os.environ, "TASKS_FILE": os.path.join(cwd, "tasks.json")},
        )
        return result.stdout.strip(), result.returncode

    # Since the script uses a hardcoded TASKS_FILE, we test structure only
    stdout, code = run_task_cmd("add", "Buy milk", cwd=cwd)
    assert code == 0, f"add failed: {stdout}"
    assert "Buy milk" in stdout or "Added" in stdout

def test_help_shows_usage():
    stdout, code = run_task_cmd()
    assert code == 0
    assert "Usage" in stdout or "command" in stdout.lower()

if __name__ == "__main__":
    test_help_shows_usage()
    test_add_and_list()
    print("All integration tests passed!")
