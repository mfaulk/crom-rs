import os
import time

n = 0
for i in range(800):
    try:
        pid = os.fork()
    except OSError:
        break
    if pid == 0:
        time.sleep(2)
        os._exit(0)
    n += 1
print("forks_without_rlimit=%d" % n)
