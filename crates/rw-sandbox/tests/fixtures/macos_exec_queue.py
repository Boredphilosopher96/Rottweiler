"""Observe a capability-bearing queue across the real sandbox/helper exec chain."""
import ctypes as c
import json
import os
import select
import signal
import sys
import time

lib = c.CDLL('/usr/lib/libSystem.B.dylib')

def task():
    return c.c_uint.in_dll(lib, 'mach_task_self_').value

class Descriptor(c.Structure):
    _fields_ = [('name', c.c_uint), ('pad1', c.c_uint), ('pad2', c.c_ushort),
                ('disposition', c.c_ubyte), ('kind', c.c_ubyte)]

class Transfer(c.Structure):
    _fields_ = [('header', c.c_uint * 6), ('count', c.c_uint), ('port', Descriptor)]

assert c.sizeof(Transfer) == 40

def transfer(destination, right, disposition, identity):
    message = Transfer((c.c_uint * 6)(0x80000013, 40, destination, 0, 0, identity),
                       1, Descriptor(right, 0, 0, disposition, 0))
    assert lib.mach_msg(c.byref(message), 17, 40, 0, 0, 1000, 0) == 0

def receive(port):
    message = (c.c_uint * 256)()
    assert lib.mach_msg(c.byref(message), 258, 0, c.sizeof(message), port, 5000, 0) == 0
    return message

def signal_effect(port):
    message = (c.c_uint * 6)(19, 24, port, 0, 0, 1122)
    assert lib.mach_msg(c.byref(message), 17, 24, 0, 0, 1000, 0) == 0

def byte(fd):
    assert select.select([fd], [], [], 10)[0], 'owned child synchronization timeout'
    return os.read(fd, 1)

endpoint = c.c_uint()
assert lib.mach_port_allocate(task(), 1, c.byref(endpoint)) == 0
assert lib.mach_port_insert_right(task(), endpoint.value, endpoint.value, 20) == 0
original = c.c_uint()
assert lib.task_get_special_port(task(), 4, c.byref(original)) == 0
assert lib.task_set_special_port(task(), 4, endpoint.value) == 0
control_read, control_write = os.pipe()
ready_read, ready_write = os.pipe()
for descriptor in [control_read, ready_write]:
    os.set_inheritable(descriptor, True)
child = os.fork()
if child == 0:
    try:
        os.close(control_write); os.close(ready_read)
        effect = c.c_uint()
        assert lib.task_get_special_port(task(), 4, c.byref(effect)) == 0
        queue = c.c_uint()
        assert lib.mach_port_allocate(task(), 1, c.byref(queue)) == 0
        assert lib.mach_port_insert_right(task(), queue.value, queue.value, 20) == 0
        # First consume the descriptor and invoke its actual endpoint as control.
        transfer(queue.value, effect.value, 19, 2233)
        baseline = receive(queue.value)
        assert baseline[5] == 2233 and baseline[6] == 1
        signal_effect(baseline[7])
        assert lib.mach_port_deallocate(task(), baseline[7]) == 0
        # The second message deliberately remains queued, carrying the same right.
        transfer(queue.value, effect.value, 19, 3344)
        transfer(effect.value, queue.value, 20, 4455)
        assert byte(control_read) == b'E'
        # This test isolates ordinary queued rights; special-root inheritance is
        # tested separately against a serving, parent-owned bootstrap endpoint.
        assert lib.task_set_special_port(task(), 4, 0) == 0
        command = json.loads(sys.argv[1])
        # The eventual target remains alive until the parent checks queue death.
        target = ("import os; "
                  f"os.write({ready_write}, b'T'); "
                  f"assert os.read({control_read}, 1) == b'F'")
        command.extend(['-c', target])
        os.execv(command[0], command)
    except BaseException:
        import traceback
        traceback.print_exc()
        os._exit(125)

os.close(control_read); os.close(ready_write)
assert lib.task_set_special_port(task(), 4, original.value) == 0
reaped = False
try:
    baseline = receive(endpoint.value)
    assert baseline[5] == 1122, 'queued capability did not reach its real parent endpoint'
    receipt = receive(endpoint.value)
    assert receipt[5] == 4455 and receipt[6] == 1
    queue_right = receipt[7]
    kind = c.c_uint()
    assert lib.mach_port_type(task(), queue_right, c.byref(kind)) == 0
    assert kind.value & 0x10000, ('queue not live before exec', kind.value)
    os.write(control_write, b'E')
    assert byte(ready_read) == b'T', 'target did not reach live checkpoint'
    assert lib.mach_port_type(task(), queue_right, c.byref(kind)) == 0
    assert kind.value == 0x100000, ('old receive queue survived exec', kind.value)
    # Zero-time receive verifies the queued descriptor did not invoke the parent.
    empty = (c.c_uint * 256)()
    assert lib.mach_msg(c.byref(empty), 258, 0, c.sizeof(empty), endpoint.value, 0, 0) == 0x10004003
    os.write(control_write, b'F')
    for _ in range(100):
        pid, status = os.waitpid(child, os.WNOHANG)
        if pid:
            reaped = True
            assert os.waitstatus_to_exitcode(status) == 0
            break
        time.sleep(0.01)
    assert reaped, 'target did not exit'
    print('actual endpoint baseline received; capability queue dead while replacement target alive')
finally:
    if not reaped:
        os.kill(child, signal.SIGKILL)
        os.waitpid(child, 0)
    for descriptor in [control_write, ready_read]:
        os.close(descriptor)
    assert lib.mach_port_destroy(task(), endpoint.value) == 0
