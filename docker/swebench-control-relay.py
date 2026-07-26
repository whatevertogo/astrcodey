#!/usr/bin/env python3
"""Fixed-target TCP relay from the trusted host-control network to one solver."""

import selectors
import socket
import sys
import threading


TARGET_HOST = sys.argv[1]
TARGET_PORT = int(sys.argv[2])


def relay(client):
    upstream = socket.create_connection((TARGET_HOST, TARGET_PORT), timeout=10)
    client.setblocking(False)
    upstream.setblocking(False)
    selector = selectors.DefaultSelector()
    selector.register(client, selectors.EVENT_READ, upstream)
    selector.register(upstream, selectors.EVENT_READ, client)
    try:
        while True:
            events = selector.select(timeout=600)
            if not events:
                return
            for key, _ in events:
                data = key.fileobj.recv(64 * 1024)
                if not data:
                    return
                key.data.sendall(data)
    finally:
        selector.close()
        upstream.close()
        client.close()


listener = socket.create_server(("0.0.0.0", 3847), reuse_port=True)


def handle(connection):
    try:
        relay(connection)
    except OSError:
        connection.close()


while True:
    connection, _ = listener.accept()
    threading.Thread(target=handle, args=(connection,), daemon=True).start()
