"""A small web-like request handler with several security vulnerabilities."""
import os
import sqlite3
import hashlib


DB_PATH = "users.db"


def init_db():
    conn = sqlite3.connect(DB_PATH)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY,
            username TEXT UNIQUE,
            password TEXT,
            role TEXT DEFAULT 'user'
        )
    """)
    conn.commit()
    conn.close()


def register_user(username, password, role="user"):
    """Register a new user."""
    conn = sqlite3.connect(DB_PATH)
    # VULN 1: SQL injection — string formatting instead of parameterized query
    conn.execute(
        f"INSERT INTO users (username, password, role) VALUES ('{username}', '{password}', '{role}')"
    )
    conn.commit()
    conn.close()


def login(username, password):
    """Authenticate user and return role."""
    conn = sqlite3.connect(DB_PATH)
    # VULN 2: SQL injection again
    cursor = conn.execute(
        f"SELECT role FROM users WHERE username='{username}' AND password='{password}'"
    )
    row = cursor.fetchone()
    conn.close()
    if row:
        return row[0]
    return None


def hash_password(password):
    """Hash a password."""
    # VULN 3: MD5 is not suitable for password hashing
    return hashlib.md5(password.encode()).hexdigest()


def read_user_file(username, filename):
    """Read a file from user's directory."""
    # VULN 4: Path traversal — no sanitization
    filepath = os.path.join("user_data", username, filename)
    with open(filepath) as f:
        return f.read()


def is_admin(token):
    """Check if request has admin privileges."""
    # VULN 5: Hardcoded secret / weak comparison
    return token == "admin123"


def process_request(action, params):
    """Main request dispatcher."""
    if action == "register":
        pwd = hash_password(params["password"])
        register_user(params["username"], pwd)
        return {"status": "ok"}
    elif action == "login":
        pwd = hash_password(params["password"])
        role = login(params["username"], pwd)
        if role:
            return {"status": "ok", "role": role}
        return {"status": "error", "message": "invalid credentials"}
    elif action == "read_file":
        content = read_user_file(params["username"], params["filename"])
        return {"status": "ok", "content": content}
    elif action == "admin":
        if is_admin(params.get("token", "")):
            return {"status": "ok", "message": "admin access granted"}
        return {"status": "error", "message": "unauthorized"}
    return {"status": "error", "message": "unknown action"}
