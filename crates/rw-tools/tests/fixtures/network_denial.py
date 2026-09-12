import errno, os, socket, sys
if any(os.environ.get(k) for k in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy")):
    sys.exit(94)
try:
    socket.socket(socket.AF_INET, socket.SOCK_STREAM)
except OSError as error:
    sys.exit(0 if error.errno in (errno.EPERM, errno.EACCES) else 93)
sys.exit(92)
