/* The test app of ../src/main.rs in C, for platforms the Rust build is not run on (macOS).
 * It reads the file with the logical name `in`, writes it upper-cased to `out`, and
 * finishes the way `boinc_finish` does, without linking the BOINC library.
 *
 *   cc -O2 -o upper upper.c
 *
 * `--seconds <n>` sleeps first, to make a job take that long. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* boinc_resolve_filename: a slot file is either the file itself or a one-line pointer into the
 * project directory. */
static void resolve(const char *logical, char *out, size_t cap) {
    char buf[4096];
    snprintf(out, cap, "%s", logical);
    FILE *f = fopen(logical, "rb");
    if (!f) return;
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = 0;
    const char *open = "<soft_link>", *close = "</soft_link>";
    if (strncmp(buf, open, strlen(open)) != 0) return;
    char *start = buf + strlen(open);
    char *end = strstr(start, close);
    if (!end) return;
    *end = 0;
    while (*start == ' ' || *start == '\n' || *start == '\r' || *start == '\t') start++;
    size_t len = strlen(start);
    while (len > 0 && strchr(" \n\r\t", start[len - 1])) start[--len] = 0;
    snprintf(out, cap, "%s", start);
}

static int run(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--seconds") == 0) {
            if (i + 1 >= argc) { fprintf(stderr, "cb-test-app: --seconds needs a number\n"); return 1; }
            double s = atof(argv[++i]);
            if (s < 0) s = 0;
            if (s > 3600) s = 3600;
            usleep((useconds_t)(s * 1e6));
        }
    }
    char path[4096];
    resolve("in", path, sizeof path);
    FILE *in = fopen(path, "rb");
    if (!in) { fprintf(stderr, "cb-test-app: in: cannot open\n"); return 1; }
    resolve("out", path, sizeof path);
    FILE *out = fopen(path, "wb");
    if (!out) { fclose(in); fprintf(stderr, "cb-test-app: out: cannot open\n"); return 1; }
    int c, failed = 0;
    while ((c = fgetc(in)) != EOF) {
        if (fputc(c >= 'a' && c <= 'z' ? c - 32 : c, out) == EOF) {
            failed = 1;
            break;
        }
    }
    if (ferror(in)) failed = 1;
    fclose(in);
    /* Buffered output can fail when it is flushed. */
    if (fclose(out) != 0) failed = 1;
    if (failed) {
        fprintf(stderr, "cb-test-app: read or write failed\n");
        return 1;
    }
    return 0;
}

int main(int argc, char **argv) {
    int status = run(argc, argv);
    /* What boinc_finish leaves for the client: the exit status, a message line, and a line
     * saying whether the message is a notice. */
    FILE *f = fopen("boinc_finish_called", "wb");
    if (f) {
        if (fprintf(f, "%d\n\n", status) < 0 || fclose(f) != 0) status = status ? status : 1;
    }
    return status;
}
