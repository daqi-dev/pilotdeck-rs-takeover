#!/usr/bin/env python3
"""POSIX owner for one native process group; fd 3 reports native status only.

The supervisor remains the session/group leader until its final group kill.
Its native child cannot inherit the private status fd or control stdin.
"""
import json
import os
import selectors
import signal
import subprocess
import sys


def finish_group(*_):
    # This process is still the live leader at the instant of the sole group
    # signal. No caller later signals a reaped/reusable PID or group number.
    if os.getpgrp() != os.getpid():
        os._exit(126)
    os.killpg(os.getpgrp(), signal.SIGKILL)
    os._exit(126)  # Defensive: SIGKILL must not return to normal execution.


def status(value):
    message = (json.dumps(value) + '\n').encode()
    while message:
        written = os.write(3, message)
        message = message[written:]


def main():
    if os.name != 'posix' or os.getpgrp() != os.getpid():
        raise RuntimeError('FunASR supervisor requires an owned POSIX session')
    os.set_inheritable(3, False)
    signal.signal(signal.SIGTERM, finish_group)
    signal.signal(signal.SIGINT, finish_group)
    selector = selectors.DefaultSelector()
    selector.register(0, selectors.EVENT_READ)
    # A pending cancellation/parent EOF wins before starting an executable.
    if selector.select(0):
        finish_group()
    if len(sys.argv) < 3 or sys.argv[1] != '--':
        raise RuntimeError('native executable argument is required')
    child = subprocess.Popen(sys.argv[2:], stdin=subprocess.DEVNULL, close_fds=True)
    reported = False
    while True:
        if not reported:
            code = child.poll()
            if code is not None:
                status({'nativeExit': True, 'code': code if code >= 0 else None,
                        'signal': signal.Signals(-code).name if code < 0 else None})
                reported = True
        # The leader intentionally stays alive after native exit until its
        # parent has received status and asks it to close all group members.
        if selector.select(0.05):
            os.read(0, 4096)  # Either a stop byte or EOF ends ownership.
            finish_group()


if __name__ == '__main__':
    try:
        main()
    except BaseException as error:
        try:
            status({'supervisorError': str(error)})
        except BaseException:
            pass
        # Child-start/control/status failures must not orphan a native group.
        finish_group()
