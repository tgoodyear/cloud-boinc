// The parser oracle, client side.
//
// Runs the BOINC client's own code on a reply file and prints what the
// client understood, one `key=value` line per field. Nothing here parses
// XML: every field printed is one the client's classes filled in. See
// cloud/docs/compatibility.md.
//
//   oracle --kind scheduler_reply --file reply.xml [--platform NAME]...
//          [--gpu nvidia|amd|intel|apple]... [--code-sign-key FILE]
//          [--file-md5 NAME=HEX]... [--master-url URL] [--prior-reply FILE]...
//   oracle --kind upload_reply    --file reply.xml
//   oracle --kind master          --file master.html
//   oracle --kind project_config|lookup_account|create_account --file reply
//
// The first three kinds are handled here with the client's classes. The
// web RPC kinds go through the client core's GUI RPC handler and then to
// oracle-gui, which holds the GUI RPC client side of lib/: those two parts
// of upstream cannot be linked into one binary (see the Makefile).

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <tuple>
#include <vector>

#include <fcntl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include "io_util.h"
#include "cpp.h"
#include "config.h"

#include "crypt.h"
#include "error_numbers.h"
#include "filesys.h"
#include "parse.h"
#include "prefs.h"
#include "str_util.h"
#include "util.h"

// SCHEDULER_OP keeps the members the master file and reply paths use
// private, and they are reached only through its poll() state machine. The
// oracle sets that state directly to run the real poll(), so it makes the
// private members of the client headers visible in this file. This changes
// no layout and no code of the compiled upstream objects.
#define private public
#include "client_state.h"
#include "scheduler_op.h"
#undef private

#include "acct_setup.h"
#include "client_msgs.h"
#include "client_types.h"
#include "file_names.h"
#include "file_xfer.h"
#include "gui_rpc_server.h"
#include "http_curl.h"
#include "log_flags.h"
#include "md5_file.h"
#include "project.h"
#include "result.h"

using std::string;
using std::vector;

static FILE* out;

// ---------------------------------------------------------------- output

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
    fprintf(out, "%s=%s\n", key.c_str(), esc(val).c_str());
}

static void emit(const string& key, const char* val) {
    emit(key, string(val ? val : ""));
}

static void emit(const string& key, double val) {
    char buf[64];
    snprintf(buf, sizeof(buf), "%.17g", val);
    emit(key, string(buf));
}

static void emit(const string& key, int val) {
    emit(key, string(std::to_string(val)));
}

static void emit_bool(const string& key, bool val) {
    emit(key, string(val ? "1" : "0"));
}

static string idx(const string& base, size_t i) {
    return base + "[" + std::to_string(i) + "]";
}

static void emit_rc(const string& key, int rc) {
    emit(key, rc);
    emit(key + "_name", rc ? boincerror(rc) : "success");
}

// ---------------------------------------------------------------- helpers

struct Options {
    string kind, file, master_url = "https://oracle.example/";
    vector<string> platforms, gpus, md5s, priors;
    string code_sign_key_file;
    int socket_buffer = 0;
};

static void usage() {
    fprintf(stderr,
        "usage: oracle --kind scheduler_reply|upload_reply|master|"
        "project_config|lookup_account|create_account --file PATH\n"
        "  [--master-url URL] [--platform NAME]... [--gpu TYPE]...\n"
        "  [--code-sign-key FILE] [--file-md5 NAME=HEX]... [--prior-reply FILE]...\n"
        "  [--socket-buffer BYTES]\n"
    );
    exit(2);
}

static Options parse_args(int argc, char** argv) {
    Options o;
    for (int i = 1; i < argc; i++) {
        string a = argv[i];
        auto next = [&]() -> string {
            if (i + 1 >= argc) usage();
            return argv[++i];
        };
        if (a == "--kind") o.kind = next();
        else if (a == "--file") o.file = next();
        else if (a == "--master-url") o.master_url = next();
        else if (a == "--platform") o.platforms.push_back(next());
        else if (a == "--gpu") o.gpus.push_back(next());
        else if (a == "--file-md5") o.md5s.push_back(next());
        else if (a == "--prior-reply") o.priors.push_back(next());
        else if (a == "--code-sign-key") o.code_sign_key_file = next();
        else if (a == "--socket-buffer") o.socket_buffer = atoi(next().c_str());
        else usage();
    }
    if (o.kind.empty() || o.file.empty()) usage();
    return o;
}

static string read_all(const string& path) {
    string s;
    FILE* f = fopen(path.c_str(), "rb");
    if (!f) {
        fprintf(stderr, "cannot open %s: %s\n", path.c_str(), strerror(errno));
        exit(2);
    }
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof(buf), f)) > 0) s.append(buf, n);
    fclose(f);
    return s;
}

// Replaces an input path by its absolute form; the oracle changes into a
// temporary directory before it reads anything.
static bool resolve_path(string& path) {
    char* real = realpath(path.c_str(), NULL);
    if (!real) {
        fprintf(stderr, "cannot open %s: %s\n", path.c_str(), strerror(errno));
        return false;
    }
    path = real;
    free(real);
    return true;
}

static void write_all(const string& path, const string& data) {
    FILE* f = fopen(path.c_str(), "wb");
    if (!f) {
        fprintf(stderr, "cannot write %s: %s\n", path.c_str(), strerror(errno));
        exit(2);
    }
    fwrite(data.data(), 1, data.size(), f);
    fclose(f);
}

static const char* prio_name(int p) {
    switch (p) {
    case MSG_INFO: return "info";
    case MSG_USER_ALERT: return "user_alert";
    case MSG_INTERNAL_ERROR: return "error";
    case MSG_SCHEDULER_ALERT: return "scheduler_alert";
    }
    return "other";
}

// Everything the client logged through msg_printf, oldest first.
static void emit_client_messages() {
    size_t n = message_descs.msgs.size();
    emit("client_msg.count", (int)n);
    for (size_t i = 0; i < n; i++) {
        MESSAGE_DESC* m = message_descs.msgs[n - 1 - i];
        emit(idx("client_msg", i) + ".priority", prio_name(m->priority));
        emit(idx("client_msg", i) + ".text", m->message);
    }
}

static string join(const vector<string>& v) {
    string r;
    for (size_t i = 0; i < v.size(); i++) {
        if (i) r += ",";
        r += v[i];
    }
    return r;
}

// ------------------------------------------------------ scheduler replies

static void emit_file_ref(const string& base, const FILE_REF& r) {
    emit(base + ".file_name", r.file_name);
    emit(base + ".open_name", r.open_name);
    emit_bool(base + ".main_program", r.main_program);
    emit_bool(base + ".copy_file", r.copy_file);
    emit_bool(base + ".optional", r.optional);
}

static void emit_resource_usage(const string& base, RESOURCE_USAGE& ru) {
    emit(base + ".avg_ncpus", ru.avg_ncpus);
    emit(base + ".flops", ru.flops);
    emit(base + ".cmdline", ru.cmdline);
    emit(base + ".rsc_type", ru.rsc_type);
    emit(base + ".coproc_usage", ru.coproc_usage);
    emit(base + ".gpu_ram", ru.gpu_ram);
    emit_bool(base + ".missing_coproc", ru.missing_coproc);
    emit(base + ".missing_coproc_name", ru.missing_coproc_name);
}

static void emit_file_info(const string& base, FILE_INFO& f) {
    emit(base + ".name", f.name);
    emit(base + ".md5_cksum", f.md5_cksum);
    emit(base + ".nbytes", f.nbytes);
    emit(base + ".max_nbytes", f.max_nbytes);
    emit(base + ".gzipped_nbytes", f.gzipped_nbytes);
    emit(base + ".status", f.status);
    emit_bool(base + ".executable", f.executable);
    emit_bool(base + ".uploaded", f.uploaded);
    emit_bool(base + ".sticky", f.sticky);
    emit(base + ".sticky_lifetime", f.sticky_lifetime);
    emit_bool(base + ".signature_required", f.signature_required);
    emit_bool(base + ".is_project_file", f.is_project_file);
    emit_bool(base + ".download_gzipped", f.download_gzipped);
    emit(base + ".download_urls", join(f.download_urls.urls));
    emit(base + ".upload_urls", join(f.upload_urls.urls));
    emit(base + ".xml_signature", f.xml_signature);
    emit(base + ".file_signature", f.file_signature);
    emit(base + ".error_msg", f.error_msg);
}

static void emit_app_version(const string& base, APP_VERSION& av) {
    emit(base + ".app_name", av.app_name);
    emit(base + ".version_num", av.version_num);
    emit(base + ".platform", av.platform);
    emit(base + ".plan_class", av.plan_class);
    emit(base + ".api_version", av.api_version);
    emit(base + ".file_prefix", av.file_prefix);
    emit_bool(base + ".needs_network", av.needs_network);
    emit_bool(base + ".dont_throttle", av.dont_throttle);
    emit_bool(base + ".is_wrapper", av.is_wrapper);
    emit_resource_usage(base, av.resource_usage);
    emit(base + ".file_ref.count", (int)av.app_files.size());
    for (size_t j = 0; j < av.app_files.size(); j++) {
        emit_file_ref(idx(base + ".file_ref", j), av.app_files[j]);
    }
}

static void emit_workunit(const string& base, WORKUNIT& wu) {
    emit(base + ".name", wu.name);
    emit(base + ".app_name", wu.app_name);
    emit(base + ".version_num", wu.version_num);
    emit(base + ".command_line", wu.command_line);
    emit(base + ".rsc_fpops_est", wu.rsc_fpops_est);
    emit(base + ".rsc_fpops_bound", wu.rsc_fpops_bound);
    emit(base + ".rsc_memory_bound", wu.rsc_memory_bound);
    emit(base + ".rsc_disk_bound", wu.rsc_disk_bound);
    emit(base + ".plan_class", wu.plan_class);
    emit_bool(base + ".has_resource_usage", wu.has_resource_usage);
    emit(base + ".input_file.count", (int)wu.input_files.size());
    for (size_t j = 0; j < wu.input_files.size(); j++) {
        emit_file_ref(idx(base + ".input_file", j), wu.input_files[j]);
    }
}

static void emit_result(const string& base, RESULT& r) {
    emit(base + ".name", r.name);
    emit(base + ".wu_name", r.wu_name);
    emit(base + ".report_deadline", r.report_deadline);
    emit(base + ".platform", r.platform);
    emit(base + ".plan_class", r.plan_class);
    emit(base + ".version_num", r.version_num);
    emit_bool(base + ".report_immediately", r.report_immediately);
    emit(base + ".output_file.count", (int)r.output_files.size());
    for (size_t j = 0; j < r.output_files.size(); j++) {
        emit_file_ref(idx(base + ".output_file", j), r.output_files[j]);
    }
}

static void emit_names(const string& base, vector<RESULT>& v) {
    emit(base + ".count", (int)v.size());
    for (size_t i = 0; i < v.size(); i++) emit(idx(base, i), v[i].name);
}

// What SCHEDULER_REPLY::parse left in the reply and in the PROJECT.
static void emit_parsed_reply(SCHEDULER_REPLY& sr, PROJECT& p) {
    emit("hostid", sr.hostid);
    emit("request_delay", sr.request_delay);
    emit("next_rpc_delay", sr.next_rpc_delay);
    emit("master_url", sr.master_url);
    emit("host_venue", sr.host_venue);
    emit("scheduler_version", sr.scheduler_version);
    emit_bool("dont_use_dcf", sr.dont_use_dcf);
    emit_bool("message_ack", sr.message_ack);
    emit_bool("project_is_down", sr.project_is_down);
    emit_bool("send_full_workload", sr.send_full_workload);
    emit("send_time_stats_log", sr.send_time_stats_log);
    emit("send_job_log", sr.send_job_log);
    emit_bool("too_recent", sr.too_recent);

    emit("message.count", (int)sr.messages.size());
    for (size_t i = 0; i < sr.messages.size(); i++) {
        emit(idx("message", i) + ".priority", sr.messages[i].priority);
        emit(idx("message", i) + ".text", sr.messages[i].message);
    }

    emit_bool("global_prefs.present", sr.global_prefs_xml != NULL);
    if (sr.global_prefs_xml) {
        emit("global_prefs.mod_time",
            GLOBAL_PREFS::parse_mod_time(sr.global_prefs_xml)
        );
    }
    emit_bool("project_prefs.present", sr.project_prefs_xml != NULL);
    if (sr.project_prefs_xml) emit("project_prefs.xml", sr.project_prefs_xml);
    emit_bool("code_sign_key.present", sr.code_sign_key != NULL);
    if (sr.code_sign_key) emit("code_sign_key.text", sr.code_sign_key);
    emit_bool("code_sign_key_signature.present", sr.code_sign_key_signature != NULL);
    if (sr.code_sign_key_signature) {
        emit("code_sign_key_signature.text", sr.code_sign_key_signature);
    }

    emit("project.name", p.project_name);
    emit("project.userid", p.userid);
    emit("project.user_name", p.user_name);
    emit("project.user_total_credit", p.user_total_credit);
    emit("project.user_expavg_credit", p.user_expavg_credit);
    emit("project.user_create_time", p.user_create_time);
    emit("project.cpid_time", p.cpid_time);
    emit("project.email_hash", p.email_hash);
    emit("project.cross_project_id", p.cross_project_id);
    emit("project.external_cpid", p.external_cpid);
    emit("project.teamid", p.teamid);
    emit("project.team_name", p.team_name);
    emit("project.host_total_credit", p.host_total_credit);
    emit("project.host_expavg_credit", p.host_expavg_credit);
    emit("project.host_create_time", p.host_create_time);
    emit("project.desired_disk_usage", p.desired_disk_usage);
    emit("project.symstore", p.symstore);
    emit("project.gui_urls", p.gui_urls);
    emit_bool("project.ended", p.ended);
    emit_bool("project.non_cpu_intensive", p.non_cpu_intensive);
    emit_bool("project.strict_memory_bound", p.strict_memory_bound);
    for (int i = 0; i < coprocs.n_rsc; i++) {
        emit_bool(string("project.no_rsc_apps.") + coprocs.coprocs[i].type,
            p.no_rsc_apps[i]
        );
    }

    emit("app.count", (int)sr.apps.size());
    for (size_t i = 0; i < sr.apps.size(); i++) {
        string b = idx("app", i);
        emit(b + ".name", sr.apps[i].name);
        emit(b + ".user_friendly_name", sr.apps[i].user_friendly_name);
        emit_bool(b + ".non_cpu_intensive", sr.apps[i].non_cpu_intensive);
        emit_bool(b + ".sporadic", sr.apps[i].sporadic);
        emit_bool(b + ".fraction_done_exact", sr.apps[i].fraction_done_exact);
    }
    emit("file_info.count", (int)sr.file_infos.size());
    for (size_t i = 0; i < sr.file_infos.size(); i++) {
        emit_file_info(idx("file_info", i), sr.file_infos[i]);
    }
    emit("file_delete.count", (int)sr.file_deletes.size());
    for (size_t i = 0; i < sr.file_deletes.size(); i++) {
        emit(idx("file_delete", i), sr.file_deletes[i]);
    }
    emit("app_version.count", (int)sr.app_versions.size());
    for (size_t i = 0; i < sr.app_versions.size(); i++) {
        emit_app_version(idx("app_version", i), sr.app_versions[i]);
    }
    emit("workunit.count", (int)sr.workunits.size());
    for (size_t i = 0; i < sr.workunits.size(); i++) {
        emit_workunit(idx("workunit", i), sr.workunits[i]);
    }
    emit("result.count", (int)sr.results.size());
    for (size_t i = 0; i < sr.results.size(); i++) {
        emit_result(idx("result", i), sr.results[i]);
    }
    emit_names("result_ack", sr.result_acks);
    emit_names("result_abort", sr.result_abort);
    emit_names("result_abort_if_not_started", sr.result_abort_if_not_started);
    emit("project_file.count", (int)sr.project_files.size());
    for (size_t i = 0; i < sr.project_files.size(); i++) {
        emit_file_ref(idx("project_file", i), sr.project_files[i]);
    }
}

// The checks the client makes with the project's code signing key: an
// application file's signature (CLIENT_STATE: FILE_INFO::verify_file in
// client/cs_files.cpp, which calls check_file_signature in lib/crypt.cpp)
// and a new key's signature (client/cs_scheduler.cpp, handle_scheduler_reply,
// which calls check_string_signature).
static void emit_signature_checks(SCHEDULER_REPLY& sr, const Options& o) {
    string key;
    if (!o.code_sign_key_file.empty()) {
        key = read_all(o.code_sign_key_file);
        strip_whitespace(key);
    } else if (sr.code_sign_key) {
        key = sr.code_sign_key;
    }
    for (const string& spec : o.md5s) {
        size_t eq = spec.find('=');
        if (eq == string::npos) usage();
        string name = spec.substr(0, eq), md5 = spec.substr(eq + 1);
        bool found = false;
        for (size_t i = 0; i < sr.file_infos.size(); i++) {
            FILE_INFO& f = sr.file_infos[i];
            if (name != f.name) continue;
            found = true;
            int retval;
            bool verified;
            std::tie(retval, verified) =
                check_file_signature(md5, f.file_signature, key);
            emit(idx("file_info", i) + ".signature_check_rc", retval);
            emit_bool(idx("file_info", i) + ".signature_verified", verified);
        }
        if (!found) emit("signature_check.missing_file." + name, 1);
    }
    if (sr.code_sign_key && sr.code_sign_key_signature &&
        !o.code_sign_key_file.empty()
    ) {
        int retval;
        bool valid;
        std::tie(retval, valid) = check_string_signature(
            sr.code_sign_key, sr.code_sign_key_signature, key
        );
        emit("code_sign_key_signature.check_rc", retval);
        emit_bool("code_sign_key_signature.valid", valid);
    }
}

// The client state after CLIENT_STATE::handle_scheduler_reply: the objects
// the reply created and what they were linked to.
static void emit_client_state(PROJECT& p) {
    emit("state.rpc_pending_hostid", p.hostid);
    emit("state.rpc_seqno", p.rpc_seqno);
    emit("state.next_rpc_time_set", p.next_rpc_time > 0 ? 1 : 0);
    emit("state.project_name", p.project_name);
    emit("state.master_url", p.master_url);
    emit("state.host_venue", p.host_venue);
    emit("state.user_name", p.user_name);
    emit("state.team_name", p.team_name);
    emit("state.project_prefs", p.project_prefs);
    emit_bool("state.dont_use_dcf", p.dont_use_dcf);
    emit_bool("state.send_full_workload", p.send_full_workload);
    emit("state.send_time_stats_log", p.send_time_stats_log);
    emit("state.project_file.count", (int)p.project_files.size());

    emit("state.app.count", (int)gstate.apps.size());
    for (size_t i = 0; i < gstate.apps.size(); i++) {
        string b = idx("state.app", i);
        emit(b + ".name", gstate.apps[i]->name);
        emit(b + ".user_friendly_name", gstate.apps[i]->user_friendly_name);
    }
    emit("state.file_info.count", (int)gstate.file_infos.size());
    for (size_t i = 0; i < gstate.file_infos.size(); i++) {
        emit_file_info(idx("state.file_info", i), *gstate.file_infos[i]);
        emit(idx("state.file_info", i) + ".ref_cnt", gstate.file_infos[i]->ref_cnt);
    }
    emit("state.app_version.count", (int)gstate.app_versions.size());
    for (size_t i = 0; i < gstate.app_versions.size(); i++) {
        APP_VERSION& av = *gstate.app_versions[i];
        emit_app_version(idx("state.app_version", i), av);
        emit_bool(idx("state.app_version", i) + ".linked_to_app", av.app != NULL);
        for (size_t j = 0; j < av.app_files.size(); j++) {
            emit_bool(
                idx(idx("state.app_version", i) + ".file_ref", j) + ".linked",
                av.app_files[j].file_info != NULL
            );
        }
    }
    emit("state.workunit.count", (int)gstate.workunits.size());
    for (size_t i = 0; i < gstate.workunits.size(); i++) {
        WORKUNIT& wu = *gstate.workunits[i];
        emit_workunit(idx("state.workunit", i), wu);
        emit_bool(idx("state.workunit", i) + ".linked_to_app", wu.app != NULL);
        for (size_t j = 0; j < wu.input_files.size(); j++) {
            emit_bool(
                idx(idx("state.workunit", i) + ".input_file", j) + ".linked",
                wu.input_files[j].file_info != NULL
            );
        }
    }
    emit("state.result.count", (int)gstate.results.size());
    for (size_t i = 0; i < gstate.results.size(); i++) {
        RESULT& r = *gstate.results[i];
        string b = idx("state.result", i);
        emit_result(b, r);
        emit(b + ".state", r.state());
        emit(b + ".linked_to_workunit", r.wup != NULL);
        emit(b + ".linked_to_app_version", r.avp != NULL);
        emit(b + ".got_server_ack", r.got_server_ack ? 1 : 0);
        emit(b + ".estimated_runtime_flops", r.resource_usage.flops);
        for (size_t j = 0; j < r.output_files.size(); j++) {
            emit_bool(
                idx(b + ".output_file", j) + ".linked",
                r.output_files[j].file_info != NULL
            );
        }
    }
    GLOBAL_PREFS& g = gstate.global_prefs;
    emit("state.global_prefs.mod_time", g.mod_time);
    emit("state.global_prefs.source_project", g.source_project);
    emit("state.global_prefs.cpu_usage_limit", g.cpu_usage_limit);
    emit("state.global_prefs.max_ncpus_pct", g.max_ncpus_pct);
    emit("state.global_prefs.work_buf_min_days", g.work_buf_min_days);
    emit("state.global_prefs.work_buf_additional_days", g.work_buf_additional_days);
    emit("state.global_prefs.disk_max_used_gb", g.disk_max_used_gb);
    emit("state.global_prefs.disk_max_used_pct", g.disk_max_used_pct);
    emit("state.global_prefs.disk_min_free_gb", g.disk_min_free_gb);
    emit("state.global_prefs.ram_max_used_busy_frac", g.ram_max_used_busy_frac);
    emit("state.global_prefs.ram_max_used_idle_frac", g.ram_max_used_idle_frac);
    emit("state.global_prefs.idle_time_to_run", g.idle_time_to_run);
    emit("state.global_prefs.suspend_cpu_usage", g.suspend_cpu_usage);
    emit_bool("state.global_prefs.run_if_user_active", g.run_if_user_active);
    emit_bool("state.global_prefs.run_on_batteries", g.run_on_batteries);
    emit_bool("state.global_prefs.leave_apps_in_memory", g.leave_apps_in_memory);
    emit("state.global_prefs.max_bytes_sec_down", g.max_bytes_sec_down);
    emit("state.global_prefs.max_bytes_sec_up", g.max_bytes_sec_up);
    emit_bool("state.global_prefs.cpu_times.present", g.cpu_times.present);
    emit("state.global_prefs.cpu_times.start_hour", g.cpu_times.start_hour);
    emit("state.global_prefs.cpu_times.end_hour", g.cpu_times.end_hour);
    emit("state.global_prefs.max_ncpus", g.max_ncpus);
    emit("state.global_prefs.daily_xfer_limit_mb", g.daily_xfer_limit_mb);
}

// The host the client runs on: its platforms and GPUs. A GPU named with
// --gpu is one with the libraries the client's plan class check asks for
// (RESOURCE_USAGE::check_gpu_libs): CUDA and OpenCL for NVIDIA, CAL and
// OpenCL for AMD, OpenCL for the others.
static void setup_host(const Options& o) {
    for (const string& g : o.gpus) {
        const char* type = NULL;
        COPROC& c = coprocs.coprocs[coprocs.n_rsc];
        c.have_opencl = true;
        if (g == "nvidia") {
            type = GPU_TYPE_NVIDIA;
            c.have_cuda = true;
        } else if (g == "amd") {
            type = GPU_TYPE_ATI;
            c.have_cal = true;
        } else if (g == "intel") {
            type = GPU_TYPE_INTEL;
        } else if (g == "apple") {
            type = GPU_TYPE_APPLE;
        } else {
            usage();
        }
        safe_strcpy(c.type, type);
        coprocs.n_rsc++;
    }
    if (o.platforms.empty()) {
        gstate.detect_platforms();
    } else {
        for (const string& p : o.platforms) gstate.add_platform(p.c_str());
    }
}

// The upload request the client sends for each file it holds that has an
// upload URL: FILE_XFER::init_upload (client/file_xfer.cpp) builds the
// request text, which the upload handler must accept. The client has not
// produced the files, so each gets 10 bytes of contents.
static void emit_upload_requests() {
    size_t n = 0;
    for (size_t i = 0; i < gstate.file_infos.size(); i++) {
        FILE_INFO* fip = gstate.file_infos[i];
        if (!fip->uploadable()) continue;
        char path[256];
        get_pathname(fip, path, sizeof(path));
        write_all(path, "0123456789");
        double len;
        md5_file(path, fip->md5_cksum, len);
        fip->nbytes = len;
        fip->upload_offset = 0;
        FILE_XFER fx;
        int retval = fx.init_upload(*fip);
        string b = idx("upload_request", n++);
        emit(b + ".file", fip->name);
        emit(b + ".rc", retval);
        emit(b + ".url", fip->upload_urls.urls[0]);
        emit(b + ".text", fx.header);
    }
    emit("upload_request.count", (int)n);
}

static int do_scheduler_reply(const Options& o, const string& reply_path) {
    log_flags.unparsed_xml = true;
    log_flags.sched_ops = true;
    setup_host(o);

    // First the reply alone: what SCHEDULER_REPLY::parse makes of it.
    {
        PROJECT p;
        safe_strcpy(p.master_url, o.master_url.c_str());
        SCHEDULER_REPLY sr;
        FILE* f = fopen(reply_path.c_str(), "rb");
        if (!f) return 2;
        int retval = sr.parse(f, &p);
        fclose(f);
        emit_rc("parse.rc", retval);
        emit_parsed_reply(sr, p);
        emit_signature_checks(sr, o);
    }
    emit_client_messages();
    message_descs.msgs.clear();

    // Then the same reply through the client's own handler, into a client
    // state that has just this project.
    PROJECT* p = new PROJECT;
    safe_strcpy(p->master_url, o.master_url.c_str());
    gstate.projects.push_back(p);
    gstate.now = dtime();
    make_project_dir(*p);
    char name[256];
    get_sched_reply_filename(*p, name, sizeof(name));
    string sched_url = o.master_url + "sched";
    char url_buf[256];
    safe_strcpy(url_buf, sched_url.c_str());

    // Replies the client got earlier, e.g. the one that sent the jobs a
    // later reply acknowledges.
    for (size_t i = 0; i < o.priors.size(); i++) {
        write_all(name, read_all(o.priors[i]));
        int r = gstate.handle_scheduler_reply(p, url_buf);
        emit_rc(idx("prior", i) + ".handle.rc", r);
        message_descs.msgs.clear();
    }
    log_flags.sched_op_debug = true;
    write_all(name, read_all(reply_path));
    int retval = gstate.handle_scheduler_reply(p, url_buf);
    emit_rc("handle.rc", retval);
    emit_client_state(*p);
    emit_upload_requests();
    emit_client_messages();
    return 0;
}

// ---------------------------------------------------------- upload replies

static int do_upload_reply(const Options& o, const string& reply_path) {
    log_flags.file_xfer_debug = false;
    PROJECT p;
    safe_strcpy(p.master_url, o.master_url.c_str());
    FILE_INFO fi;
    fi.project = &p;
    safe_strcpy(fi.name, "file");
    string body = read_all(reply_path);
    // The HTTP layer leaves the body of the reply in req1
    // (HTTP_OP::init_post2).
    vector<char> buf(body.begin(), body.end());
    buf.push_back(0);
    FILE_XFER fx;
    fx.fip = &fi;
    fx.req1 = buf.data();
    double nbytes = 0;
    int retval = fx.parse_upload_response(nbytes);
    emit_rc("parse.rc", retval);
    emit("file_size", nbytes);
    emit_client_messages();
    return 0;
}

// ------------------------------------------------------------ master files

static int do_master(const Options& o, const string& reply_path) {
    log_flags.sched_op_debug = false;
    PROJECT* p = new PROJECT;
    safe_strcpy(p->master_url, o.master_url.c_str());
    p->master_url_fetch_pending = true;
    gstate.projects.push_back(p);
    gstate.now = dtime();
    char name[256];
    get_master_filename(*p, name, sizeof(name));
    write_all(name, read_all(reply_path));

    // The client's poll() reads the master file after the HTTP fetch
    // completes; set the state a finished fetch leaves and run it.
    HTTP_OP_SET http_ops;
    SCHEDULER_OP op(&http_ops);
    op.cur_proj = p;
    op.state = SCHEDULER_OP_STATE_GET_MASTER;
    op.http_op.http_op_state = HTTP_STATE_DONE;
    op.http_op.http_op_retval = 0;
    op.poll();

    emit("master_fetch_failures", p->master_fetch_failures);
    emit_bool("master_url_fetch_pending", p->master_url_fetch_pending);
    emit("scheduler_url.count", (int)p->scheduler_urls.size());
    for (size_t i = 0; i < p->scheduler_urls.size(); i++) {
        emit(idx("scheduler_url", i), p->scheduler_urls[i]);
    }
    emit_client_messages();
    return 0;
}

// ---------------------------------------------------------------- web RPCs

// The client core keeps the reply of a web RPC as a string and gives it to
// the GUI in answer to a *_poll RPC (client/gui_rpc_server_ops.cpp). Run
// that exchange over a socket pair with the real GUI_RPC_CONN, and hand the
// bytes it sent to oracle-gui.
static int do_web_rpc(const Options& o, const string& reply_path) {
    string req, filename;
    int which;
    if (o.kind == "project_config") {
        req = "<get_project_config_poll/>\n";
        filename = GET_PROJECT_CONFIG_FILENAME;
        which = 0;
    } else if (o.kind == "lookup_account") {
        req = "<lookup_account_poll/>\n";
        filename = LOOKUP_ACCOUNT_FILENAME;
        which = 1;
    } else {
        req = "<create_account_poll/>\n";
        filename = CREATE_ACCOUNT_FILENAME;
        which = 2;
    }
    write_all(filename, read_all(reply_path));

    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
        perror("socketpair");
        return 2;
    }
    set_socket_buffers(sv, o.socket_buffer);
    GUI_RPC_CONN* grc = new GUI_RPC_CONN(sv[0]);
    grc->is_local = true;
    // What the *_OP::handle_reply of client/acct_setup.cpp does when the
    // HTTP transfer finished without error.
    switch (which) {
    case 0: grc->get_project_config_op.handle_reply(0); break;
    case 1: grc->lookup_account_op.handle_reply(0); break;
    case 2: grc->create_account_op.handle_reply(0); break;
    }
    string request = "<boinc_gui_rpc_request>\n" + req +
        "</boinc_gui_rpc_request>\n\003";
    if (!write_fully(sv[1], request.data(), request.size())) return 2;

    // handle_rpc() sends the whole reply with a blocking send(), which
    // waits for a reader once the socket buffer is full. Read on another
    // thread so that it does not wait for this one.
    string reply;
    bool read_ok = false;
    std::thread reader([&]() {
        read_ok = read_message(sv[1], reply);
        // A reader that gave up must not leave the sender blocked.
        if (!read_ok) shutdown(sv[1], SHUT_RDWR);
    });
    grc->handle_rpc();
    reader.join();
    if (!read_ok) {
        fprintf(stderr, "oracle: no complete reply from the client core\n");
        return 2;
    }
    write_all("gui_reply.bin", reply);
    emit("core.reply_bytes", (int)reply.size());
    fflush(out);

    pid_t pid = fork();
    watchdog_child = pid;
    if (pid == 0) {
        // oracle-gui sits next to this binary.
        char self[4096];
        ssize_t len = readlink("/proc/self/exe", self, sizeof(self) - 1);
        if (len < 0) _exit(2);
        self[len] = 0;
        string exe = self;
        string gui = exe.substr(0, exe.rfind('/')) + "/oracle-gui";
        dup2(fileno(out), 1);
        char path[4096];
        string gui_reply = string(getcwd(path, sizeof(path))) + "/gui_reply.bin";
        string buffer = std::to_string(o.socket_buffer);
        execl(gui.c_str(), gui.c_str(), "--kind", o.kind.c_str(),
            "--file", gui_reply.c_str(), "--socket-buffer", buffer.c_str(),
            (char*)NULL
        );
        perror("exec oracle-gui");
        _exit(2);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 2;
}

int main(int argc, char** argv) {
    watchdog_start();
    Options o = parse_args(argc, argv);

    // The client prints its messages to stdout; the dump goes elsewhere.
    int fd = dup(1);
    dup2(2, 1);
    out = fdopen(fd, "w");

    // Every input path is made absolute here, before the chdir below.
    string path = o.file;
    if (!resolve_path(path)) return 2;
    if (!o.code_sign_key_file.empty() && !resolve_path(o.code_sign_key_file)) {
        return 2;
    }
    for (size_t i = 0; i < o.priors.size(); i++) {
        if (!resolve_path(o.priors[i])) return 2;
    }

    // The client writes files (statistics, preferences, master and reply
    // copies) into its working directory.
    char tmpl[] = "/tmp/parser-oracle-XXXXXX";
    if (!mkdtemp(tmpl)) {
        perror("mkdtemp");
        return 2;
    }
    string tmpdir = tmpl;
    snprintf(watchdog_dir, sizeof(watchdog_dir), "%s", tmpl);
    if (chdir(tmpdir.c_str())) {
        perror("chdir");
        return 2;
    }

    // The client does this once at startup (client/main.cpp); file
    // transfers need it.
    curl_init();

    int rc;
    if (o.kind == "scheduler_reply") rc = do_scheduler_reply(o, path);
    else if (o.kind == "upload_reply") rc = do_upload_reply(o, path);
    else if (o.kind == "master") rc = do_master(o, path);
    else if (o.kind == "project_config" || o.kind == "lookup_account" ||
        o.kind == "create_account"
    ) {
        rc = do_web_rpc(o, path);
    } else {
        usage();
        rc = 2;
    }
    fflush(out);

    string cmd = "rm -rf '" + tmpdir + "'";
    if (system(cmd.c_str())) {}
    return rc;
}
