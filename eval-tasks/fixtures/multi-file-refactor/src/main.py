"""A task manager with everything jammed in one file that needs splitting."""
import json
import os
from datetime import datetime


# --- Storage ---
TASKS_FILE = "tasks.json"

def load_tasks():
    if not os.path.exists(TASKS_FILE):
        return []
    with open(TASKS_FILE) as f:
        return json.load(f)

def save_tasks(tasks):
    with open(TASKS_FILE, 'w') as f:
        json.dump(tasks, f, indent=2, default=str)


# --- Task operations ---
def add_task(title, priority="medium"):
    tasks = load_tasks()
    task = {
        "id": len(tasks) + 1,
        "title": title,
        "priority": priority,
        "done": False,
        "created_at": datetime.now().isoformat(),
    }
    tasks.append(task)
    save_tasks(tasks)
    return task

def complete_task(task_id):
    tasks = load_tasks()
    for task in tasks:
        if task["id"] == task_id:
            task["done"] = True
            task["completed_at"] = datetime.now().isoformat()
            save_tasks(tasks)
            return True
    return False

def list_tasks(show_done=False):
    tasks = load_tasks()
    if not show_done:
        tasks = [t for t in tasks if not t["done"]]
    return tasks

def delete_task(task_id):
    tasks = load_tasks()
    tasks = [t for t in tasks if t["id"] != task_id]
    save_tasks(tasks)


# --- Display ---
def format_task(task):
    status = "✓" if task["done"] else "○"
    priority_markers = {"high": "!!!", "medium": "!!", "low": "!"}
    marker = priority_markers.get(task["priority"], "")
    return f"  {status} [{task['id']}] {task['title']} {marker}"

def print_task_list(tasks):
    if not tasks:
        print("  No tasks.")
        return
    for task in sorted(tasks, key=lambda t: {"high": 0, "medium": 1, "low": 2}.get(t["priority"], 3)):
        print(format_task(task))


# --- CLI ---
def main():
    import sys
    if len(sys.argv) < 2:
        print("Usage: task <command> [args]")
        print("Commands: add, done, list, delete")
        return

    cmd = sys.argv[1]
    if cmd == "add":
        title = " ".join(sys.argv[2:]) if len(sys.argv) > 2 else input("Title: ")
        priority = "medium"
        if "--high" in sys.argv:
            priority = "high"
        elif "--low" in sys.argv:
            priority = "low"
        title = title.replace("--high", "").replace("--low", "").strip()
        task = add_task(title, priority)
        print(f"Added: {format_task(task)}")
    elif cmd == "done":
        if len(sys.argv) < 3:
            print("Usage: task done <id>")
            return
        task_id = int(sys.argv[2])
        if complete_task(task_id):
            print(f"Completed task {task_id}")
        else:
            print(f"Task {task_id} not found")
    elif cmd == "list":
        show_done = "--all" in sys.argv
        tasks = list_tasks(show_done)
        print_task_list(tasks)
    elif cmd == "delete":
        if len(sys.argv) < 3:
            print("Usage: task delete <id>")
            return
        task_id = int(sys.argv[2])
        delete_task(task_id)
        print(f"Deleted task {task_id}")
    else:
        print(f"Unknown command: {cmd}")

if __name__ == "__main__":
    main()
