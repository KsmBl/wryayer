// Tiny *static* helper that forwards a launch request from inside a sandbox out
// to the wryayer host portal.
//
// It is installed inside a sandbox as symlinks named after each bound app (for
// example /usr/local/bin/firefox -> /.wryayer-portal). When the sandboxed app
// runs `firefox <url>`, PATH resolves to this helper; it connects to the
// AF_UNIX socket named by $WRYAYER_PORTAL_SOCK and sends the target app name
// (the basename of argv[0]) followed by each argument, every field
// NUL-terminated. Closing the connection ends the record. The host side then
// launches `wryayer run <app> -- <args>` in that app's own container.
//
// Built with -static so it runs no matter which libraries the sandboxed app's
// filesystem tree happens to ship (the host /usr is not mounted in the sandbox).

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>

static int write_all(int fd, const char *buf, size_t len) {
    while (len > 0) {
        ssize_t n = write(fd, buf, len);
        if (n < 0) return -1;
        buf += n;
        len -= (size_t)n;
    }
    return 0;
}

// Names an app might be invoked as to "open a URL/file", where argv[1] is the
// target and the real app to launch is whatever $WRYAYER_OPEN_APP points at.
static int is_opener_alias(const char *name) {
    static const char *aliases[] = {
        "xdg-open", "x-www-browser", "www-browser", "sensible-browser",
        "gnome-open", "kde-open", "kde-open5", NULL,
    };
    for (int i = 0; aliases[i]; i++) {
        if (strcmp(name, aliases[i]) == 0) return 1;
    }
    return 0;
}

int main(int argc, char **argv) {
    const char *sock = getenv("WRYAYER_PORTAL_SOCK");
    if (!sock || !*sock) sock = "/run/wryayer/portal.sock";

    // Target app = basename of argv[0] (the symlink name we were invoked as).
    const char *app = (argc > 0 && argv[0]) ? argv[0] : "";
    const char *slash = strrchr(app, '/');
    if (slash) app = slash + 1;
    // When invoked as a generic opener (xdg-open, x-www-browser, …), forward to
    // the app the host designated as this sandbox's link handler.
    if (is_opener_alias(app)) {
        const char *open_app = getenv("WRYAYER_OPEN_APP");
        if (open_app && *open_app) app = open_app;
    }
    if (!*app) {
        fprintf(stderr, "wryayer-portal: could not determine app name\n");
        return 2;
    }

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        perror("wryayer-portal: socket");
        return 1;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    if (strlen(sock) >= sizeof(addr.sun_path)) {
        fprintf(stderr, "wryayer-portal: socket path too long\n");
        return 1;
    }
    strcpy(addr.sun_path, sock);

    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("wryayer-portal: connect");
        return 1;
    }

    // app name, then each argument, all NUL-terminated; EOF ends the record.
    if (write_all(fd, app, strlen(app) + 1) < 0) {
        perror("wryayer-portal: write");
        return 1;
    }
    for (int i = 1; i < argc; i++) {
        if (write_all(fd, argv[i], strlen(argv[i]) + 1) < 0) {
            perror("wryayer-portal: write");
            return 1;
        }
    }
    close(fd);
    return 0;
}
