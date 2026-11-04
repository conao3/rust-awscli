"""A fork server inside the reference AWS CLI.

The harness starts the reference CLI with a configuration whose
``[plugins]`` section names this module, so it is imported by
``create_clidriver`` after the CLI's own modules are loaded. It then
never returns: it serves ``AWSR_REFSERVER_SOCKET`` and forks a fresh run of
``awscli.clidriver.main`` for every request, so a case does not pay for
starting the interpreter and importing the CLI.

A request is an 8-byte big-endian length sent with the case's stdin,
stdout, and stderr descriptors, followed by that many bytes of JSON
``{"argv": [...], "env": {...}, "cwd": "..."}``. The reply is
``pid <pid>\\n`` once the run has started and ``exit <code>\\n`` or
``signal <number>\\n`` when it has ended.
"""

import atexit
import json
import os
import signal
import socket
import struct
import sys
import time


def _recv_exact(conn, n):
    data = b''
    while len(data) < n:
        chunk = conn.recv(n - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def _run_case(conn, fds, request):
    os.setpgid(0, 0)
    for target, fd in enumerate(fds):
        os.dup2(fd, target)
    for fd in fds:
        if fd > 2:
            os.close(fd)
    conn.close()
    os.chdir(request['cwd'])
    env = dict(_BASE_ENV)
    env.update(request['env'])
    os.environ.clear()
    os.environ.update(env)
    time.tzset()
    sys.argv = [sys.argv[0]] + request['argv']
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(line_buffering=os.isatty(stream.fileno()))
    from awscli.clidriver import main

    try:
        rc = main()
    except SystemExit as e:
        rc = e.code
    if rc is None:
        rc = 0
    elif not isinstance(rc, int):
        sys.stderr.write(f'{rc}\n')
        rc = 1
    atexit._run_exitfuncs()
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.flush()
        except Exception:
            pass
    os._exit(rc & 0xFF)


def _supervise(conn, fds, request):
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)
    pid = os.fork()
    if pid == 0:
        _run_case(conn, fds, request)
    for fd in fds:
        os.close(fd)
    conn.sendall(f'pid {pid}\n'.encode())
    _, status = os.waitpid(pid, 0)
    if os.WIFSIGNALED(status):
        conn.sendall(f'signal {os.WTERMSIG(status)}\n'.encode())
    else:
        conn.sendall(f'exit {os.WEXITSTATUS(status)}\n'.encode())
    os._exit(0)


def _serve(path):
    signal.signal(signal.SIGCHLD, signal.SIG_IGN)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(path)
    server.listen(128)
    sys.stdout.write('ready\n')
    sys.stdout.flush()
    while True:
        conn, _ = server.accept()
        try:
            header, fds, _, _ = socket.recv_fds(conn, 8, 3)
            (length,) = struct.unpack('>Q', header)
            request = json.loads(_recv_exact(conn, length))
        except Exception:
            conn.close()
            continue
        if os.fork() == 0:
            server.close()
            _supervise(conn, fds, request)
        for fd in fds:
            os.close(fd)
        conn.close()


_BASE_ENV = dict(os.environ)
_SOCKET = _BASE_ENV.pop('AWSR_REFSERVER_SOCKET')
_serve(_SOCKET)
