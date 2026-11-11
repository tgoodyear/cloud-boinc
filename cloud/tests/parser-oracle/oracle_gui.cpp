// The parser oracle, GUI RPC client side.
//
//   oracle-gui --kind project_config|lookup_account|create_account --file PATH
//
// PATH holds the bytes the client core sent in answer to the matching
// *_poll GUI RPC, as written by `oracle`. This program is the other end of
// that RPC: it runs the real RPC_CLIENT::get_project_config_poll,
// lookup_account_poll or create_account_poll of lib/gui_rpc_client_ops.cpp
// over a socket pair that already holds the reply, and prints what the
// PROJECT_CONFIG or ACCOUNT_OUT it fills in contains. See
// cloud/docs/compatibility.md.

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

#include "config.h"
#include "error_numbers.h"
#include "gui_rpc_client.h"

using std::string;

static string esc(const string& s) {
    string r;
    for (char c : s) {
        switch (c) {
        case '\n': r += "\\n"; break;
        case '\r': r += "\\r"; break;
        case '\\': r += "\\\\"; break;
        default: r += c;
        }
    }
    return r;
}

static void emit(const string& key, const string& val) {
    printf("%s=%s\n", key.c_str(), esc(val).c_str());
}

static void emit(const string& key, int val) {
    emit(key, std::to_string(val));
}

static void emit_bool(const string& key, bool val) {
    emit(key, string(val ? "1" : "0"));
}

static void emit_rc(const string& key, int rc) {
    emit(key, rc);
    emit(key + "_name", rc ? boincerror(rc) : "success");
}

int main(int argc, char** argv) {
    string kind, file;
    for (int i = 1; i < argc; i++) {
        string a = argv[i];
        if (a == "--kind" && i + 1 < argc) kind = argv[++i];
        else if (a == "--file" && i + 1 < argc) file = argv[++i];
        else {
            fprintf(stderr,
                "usage: oracle-gui --kind project_config|lookup_account|"
                "create_account --file PATH\n"
            );
            return 2;
        }
    }
    if (kind.empty() || file.empty()) {
        fprintf(stderr, "oracle-gui: --kind and --file are required\n");
        return 2;
    }
    FILE* f = fopen(file.c_str(), "rb");
    if (!f) {
        fprintf(stderr, "cannot open %s: %s\n", file.c_str(), strerror(errno));
        return 2;
    }
    string reply;
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof(buf), f)) > 0) reply.append(buf, n);
    fclose(f);

    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
        perror("socketpair");
        return 2;
    }
    if (write(sv[1], reply.data(), reply.size()) != (ssize_t)reply.size()) {
        perror("write");
        return 2;
    }
    RPC_CLIENT rpc;
    rpc.sock = sv[0];

    if (kind == "project_config") {
        PROJECT_CONFIG pc;
        int rc = rpc.get_project_config_poll(pc);
        emit_rc("parse.rc", rc);
        emit("config.error_num", pc.error_num);
        emit("config.error_msg", pc.error_msg);
        emit("config.name", pc.name);
        emit("config.master_url", pc.master_url);
        emit("config.web_rpc_url_base", pc.web_rpc_url_base);
        emit("config.local_revision", pc.local_revision);
        emit("config.min_passwd_length", pc.min_passwd_length);
        emit_bool("config.account_manager", pc.account_manager);
        emit_bool("config.uses_username", pc.uses_username);
        emit_bool("config.account_creation_disabled", pc.account_creation_disabled);
        emit_bool("config.client_account_creation_disabled",
            pc.client_account_creation_disabled
        );
        emit_bool("config.sched_stopped", pc.sched_stopped);
        emit_bool("config.web_stopped", pc.web_stopped);
        emit("config.min_client_version", pc.min_client_version);
        emit_bool("config.terms_of_use_is_html", pc.terms_of_use_is_html);
        emit("config.terms_of_use", pc.terms_of_use);
        emit_bool("config.ldap_auth", pc.ldap_auth);
        emit("config.platform.count", (int)pc.platforms.size());
        for (size_t i = 0; i < pc.platforms.size(); i++) {
            emit("config.platform[" + std::to_string(i) + "]", pc.platforms[i]);
        }
    } else if (kind == "lookup_account" || kind == "create_account") {
        ACCOUNT_OUT ao;
        int rc = kind == "lookup_account"
            ? rpc.lookup_account_poll(ao)
            : rpc.create_account_poll(ao);
        emit_rc("parse.rc", rc);
        emit("account.error_num", ao.error_num);
        emit("account.error_msg", ao.error_msg);
        emit("account.authenticator", ao.authenticator);
    } else {
        fprintf(stderr, "oracle-gui: unknown kind %s\n", kind.c_str());
        return 2;
    }
    return 0;
}
