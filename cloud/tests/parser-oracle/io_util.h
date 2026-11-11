// Shared by oracle.cpp and oracle_gui.cpp: the pieces that keep a socket
// exchange between a writer and a reader from hanging the process.
//
// A blocking write to a socket pair waits for its peer to read once the
// socket buffer is full, so every exchange here runs its writer and its
// reader on different threads. A watchdog ends the process with a message if
// something still waits too long.

#ifndef PARSER_ORACLE_IO_UTIL_H
#define PARSER_ORACLE_IO_UTIL_H

#include <cerrno>
#include <csignal>
#include <cstdlib>
#include <string>

#include <poll.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

// The longest the whole run may take, in seconds. CB_PARSER_ORACLE_TIMEOUT_SECS
// overrides it.
static int watchdog_seconds() {
    const char* v = getenv("CB_PARSER_ORACLE_TIMEOUT_SECS");
    int n = v ? atoi(v) : 0;
    return n > 0 ? n : 60;
}

// A child process to kill with this one, if any.
static pid_t watchdog_child = 0;

// A scratch directory to remove when the watchdog fires, if any.
static char watchdog_dir[256] = "";

static void watchdog_fire(int) {
    static const char msg[] =
        "parser oracle: timed out; the exchange with the client code did not finish\n";
    ssize_t r = write(2, msg, sizeof(msg) - 1);
    (void)r;
    if (watchdog_child > 0) kill(watchdog_child, SIGKILL);
    if (watchdog_dir[0]) {
        pid_t rm = vfork();
        if (rm == 0) {
            execl("/bin/rm", "rm", "-rf", watchdog_dir, (char*)NULL);
            _exit(127);
        }
        if (rm > 0) waitpid(rm, NULL, 0);
    }
    _exit(3);
}

// Call first in main. A write to a socket whose peer has gone then fails
// with EPIPE instead of ending the process.
static void watchdog_start() {
    signal(SIGPIPE, SIG_IGN);
    signal(SIGALRM, watchdog_fire);
    alarm(watchdog_seconds());
}

// Writes all of the data, however many writes it takes.
static bool write_fully(int fd, const char* p, size_t n) {
    while (n > 0) {
        ssize_t k = write(fd, p, n);
        if (k < 0) {
            if (errno == EINTR) continue;
            return false;
        }
        p += k;
        n -= (size_t)k;
    }
    return true;
}

// Reads from fd into data until the ETX that ends a GUI RPC message or the
// peer closes; false if nothing arrives for the watchdog period or a read
// fails.
static bool read_message(int fd, std::string& data) {
    char buf[4096];
    for (;;) {
        struct pollfd pfd = {fd, POLLIN, 0};
        int r = poll(&pfd, 1, watchdog_seconds() * 1000);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) return false;
        ssize_t n = read(fd, buf, sizeof(buf));
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) return false;
        if (n == 0) return true;
        data.append(buf, (size_t)n);
        if (data.find('\003') != std::string::npos) return true;
    }
}

// Sets the buffers of both ends of a socket pair (0 leaves the defaults).
// A small buffer makes even a short reply need a reader at the other end.
static void set_socket_buffers(int sv[2], int bytes) {
    if (bytes <= 0) return;
    for (int i = 0; i < 2; i++) {
        setsockopt(sv[i], SOL_SOCKET, SO_SNDBUF, &bytes, sizeof(bytes));
        setsockopt(sv[i], SOL_SOCKET, SO_RCVBUF, &bytes, sizeof(bytes));
    }
}

#endif
