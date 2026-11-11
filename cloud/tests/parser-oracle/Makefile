# Builds the parser oracle: the BOINC client's own parsers, compiled from
# the upstream sources in this tree, behind a small command line.
#
#   make -C cloud/tests/parser-oracle -j4
#
# The result is build/oracle. See cloud/docs/compatibility.md.

# Root of the tree that holds lib/ and client/.
TOP ?= ../../..
BUILD ?= build
CXX ?= g++
CXXFLAGS ?= -O0 -g0 -w -std=gnu++17

INCLUDES = -I. -I$(TOP) -I$(TOP)/api -I$(TOP)/db -I$(TOP)/lib -I$(TOP)/lib/mac \
	-I$(TOP)/sched -I$(TOP)/tools -I$(TOP)/vda -I$(TOP)/client
CPPFLAGS += -DHAVE_CONFIG_H $(INCLUDES)
LDLIBS = -lcurl -lssl -lcrypto -lresolv -lpthread -lm -ldl -lrt -lz

# lib/Makefile.am, generic_sources plus the Unix files, and
# libboinc_crypt (crypt.cpp).
LIB_SRCS = app_ipc base64 cc_config cert_sig coproc crypt diagnostics filesys \
	hostinfo keyword md5 md5_file mem_usage mfile miofile msg_log network \
	notice opencl_boinc parse prefs procinfo proc_control project_init \
	proxy_info sched_msgs shmem str_util url util wslinfo procinfo_unix \
	synch unix_util

# The GUI RPC client side of lib/. It defines its own APP, PROJECT, RESULT
# and other classes that clash with the client's at link time, so the
# client and this part never share a binary (upstream links them into
# boinc_client and boinccmd/boincmgr respectively).
GUI_SRCS = gui_rpc_client gui_rpc_client_ops gui_rpc_client_print

# client/Makefile.am, boinc_client_SOURCES for Linux, without main.cpp.
CLIENT_SRCS = acct_mgr acct_setup app app_config app_control app_start app_test \
	async_file check_state client_msgs client_state client_types coproc_sched \
	cpu_sched cs_account cs_apps cs_benchmark cs_cmdline cs_files cs_notice \
	cs_platforms cs_prefs cs_proxy cs_scheduler cs_sporadic cs_statefile \
	cs_trickle current_version dhrystone dhrystone2 file_names file_xfer \
	gpu_amd gpu_detect gpu_intel gpu_nvidia gpu_opencl gui_http gui_rpc_server \
	gui_rpc_server_ops hostinfo_linux hostinfo_network http_curl log_flags \
	mac_address net_stats pers_file_xfer project result rr_sim sandbox \
	scheduler_op thread time_stats whetstone work_fetch hostinfo_unix

LIB_OBJS = $(addprefix $(BUILD)/lib/,$(addsuffix .o,$(LIB_SRCS)))
GUI_OBJS = $(addprefix $(BUILD)/lib/,$(addsuffix .o,$(GUI_SRCS)))
CLIENT_OBJS = $(addprefix $(BUILD)/client/,$(addsuffix .o,$(CLIENT_SRCS)))
OBJS = $(LIB_OBJS) $(GUI_OBJS) $(CLIENT_OBJS) $(BUILD)/oracle.o $(BUILD)/oracle_gui.o

all: $(BUILD)/oracle $(BUILD)/oracle-gui

$(BUILD)/oracle: $(BUILD)/oracle.o $(CLIENT_OBJS) $(LIB_OBJS)
	$(CXX) -o $@ $^ $(LDLIBS)

$(BUILD)/oracle-gui: $(BUILD)/oracle_gui.o $(GUI_OBJS) $(LIB_OBJS)
	$(CXX) -o $@ $^ $(LDLIBS)

$(BUILD)/lib/%.o: $(TOP)/lib/%.cpp
	@mkdir -p $(dir $@)
	$(CXX) $(CXXFLAGS) $(CPPFLAGS) -MMD -c -o $@ $<

$(BUILD)/client/%.o: $(TOP)/client/%.cpp
	@mkdir -p $(dir $@)
	$(CXX) $(CXXFLAGS) $(CPPFLAGS) -MMD -c -o $@ $<

$(BUILD)/%.o: %.cpp
	@mkdir -p $(dir $@)
	$(CXX) $(CXXFLAGS) $(CPPFLAGS) -MMD -c -o $@ $<

clean:
	rm -rf $(BUILD)

-include $(OBJS:.o=.d)

.PHONY: all clean
