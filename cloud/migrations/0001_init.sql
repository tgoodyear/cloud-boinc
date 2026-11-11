-- Port of db/schema.sql for the tables cloud-boinc uses.
-- Column names and state constants follow upstream; see db/boinc_db_types.h.

CREATE TABLE config (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    mod_time INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE platform (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    name TEXT NOT NULL UNIQUE,
    user_friendly_name TEXT NOT NULL,
    deprecated INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE app (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    name TEXT NOT NULL UNIQUE,
    min_version INTEGER NOT NULL DEFAULT 0,
    deprecated INTEGER NOT NULL DEFAULT 0,
    user_friendly_name TEXT NOT NULL,
    weight REAL NOT NULL DEFAULT 1,
    beta INTEGER NOT NULL DEFAULT 0,
    target_nresults INTEGER NOT NULL DEFAULT 0,
    non_cpu_intensive INTEGER NOT NULL DEFAULT 0,
    fraction_done_exact INTEGER NOT NULL DEFAULT 0,
    -- cloud-boinc: validator for this app: 'trivial', 'bitwise' or 'webhook'
    validator TEXT NOT NULL DEFAULT 'trivial',
    -- cloud-boinc: webhook URL for the 'webhook' validator
    validator_url TEXT NOT NULL DEFAULT '',
    -- cloud-boinc: URL told about workunits that are ready to assimilate
    assimilate_url TEXT NOT NULL DEFAULT ''
);

CREATE TABLE app_version (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    appid INTEGER NOT NULL,
    version_num INTEGER NOT NULL,
    platformid INTEGER NOT NULL,
    xml_doc TEXT NOT NULL,
    min_core_version INTEGER NOT NULL DEFAULT 0,
    max_core_version INTEGER NOT NULL DEFAULT 0,
    deprecated INTEGER NOT NULL DEFAULT 0,
    plan_class TEXT NOT NULL DEFAULT '',
    pfc_n REAL NOT NULL DEFAULT 0,
    pfc_avg REAL NOT NULL DEFAULT 0,
    pfc_scale REAL NOT NULL DEFAULT 0,
    expavg_credit REAL NOT NULL DEFAULT 0,
    expavg_time REAL NOT NULL DEFAULT 0,
    beta INTEGER NOT NULL DEFAULT 0,
    UNIQUE (appid, platformid, version_num, plan_class)
);

CREATE TABLE user (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    email_addr TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL DEFAULT '',
    authenticator TEXT NOT NULL UNIQUE,
    country TEXT NOT NULL DEFAULT '',
    postal_code TEXT NOT NULL DEFAULT '',
    total_credit REAL NOT NULL DEFAULT 0,
    expavg_credit REAL NOT NULL DEFAULT 0,
    expavg_time REAL NOT NULL DEFAULT 0,
    global_prefs TEXT NOT NULL DEFAULT '',
    project_prefs TEXT NOT NULL DEFAULT '',
    teamid INTEGER NOT NULL DEFAULT 0,
    venue TEXT NOT NULL DEFAULT '',
    url TEXT NOT NULL DEFAULT '',
    send_email INTEGER NOT NULL DEFAULT 0,
    show_hosts INTEGER NOT NULL DEFAULT 0,
    cross_project_id TEXT NOT NULL DEFAULT '',
    -- cloud-boinc: HMAC of the client-side md5(password + email), not the bare hash
    passwd_hash TEXT NOT NULL DEFAULT '',
    email_validated INTEGER NOT NULL DEFAULT 0,
    -- cloud-boinc: 1 when the account may not use the scheduler
    blocked INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE host (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    userid INTEGER NOT NULL,
    rpc_seqno INTEGER NOT NULL DEFAULT 0,
    rpc_time INTEGER NOT NULL DEFAULT 0,
    total_credit REAL NOT NULL DEFAULT 0,
    expavg_credit REAL NOT NULL DEFAULT 0,
    expavg_time REAL NOT NULL DEFAULT 0,
    timezone INTEGER NOT NULL DEFAULT 0,
    domain_name TEXT NOT NULL DEFAULT '',
    serialnum TEXT NOT NULL DEFAULT '',
    last_ip_addr TEXT NOT NULL DEFAULT '',
    nsame_ip_addr INTEGER NOT NULL DEFAULT 0,
    on_frac REAL NOT NULL DEFAULT 0,
    connected_frac REAL NOT NULL DEFAULT 0,
    active_frac REAL NOT NULL DEFAULT 0,
    cpu_efficiency REAL NOT NULL DEFAULT 0,
    duration_correction_factor REAL NOT NULL DEFAULT 0,
    p_ncpus INTEGER NOT NULL DEFAULT 0,
    p_vendor TEXT NOT NULL DEFAULT '',
    p_model TEXT NOT NULL DEFAULT '',
    p_fpops REAL NOT NULL DEFAULT 0,
    p_iops REAL NOT NULL DEFAULT 0,
    p_membw REAL NOT NULL DEFAULT 0,
    os_name TEXT NOT NULL DEFAULT '',
    os_version TEXT NOT NULL DEFAULT '',
    m_nbytes REAL NOT NULL DEFAULT 0,
    m_cache REAL NOT NULL DEFAULT 0,
    m_swap REAL NOT NULL DEFAULT 0,
    d_total REAL NOT NULL DEFAULT 0,
    d_free REAL NOT NULL DEFAULT 0,
    d_boinc_used_total REAL NOT NULL DEFAULT 0,
    d_boinc_used_project REAL NOT NULL DEFAULT 0,
    d_boinc_max REAL NOT NULL DEFAULT 0,
    n_bwup REAL NOT NULL DEFAULT 0,
    n_bwdown REAL NOT NULL DEFAULT 0,
    credit_per_cpu_sec REAL NOT NULL DEFAULT 0,
    venue TEXT NOT NULL DEFAULT '',
    nresults_today INTEGER NOT NULL DEFAULT 0,
    avg_turnaround REAL NOT NULL DEFAULT 0,
    host_cpid TEXT NOT NULL DEFAULT '',
    external_ip_addr TEXT NOT NULL DEFAULT '',
    max_results_day INTEGER NOT NULL DEFAULT 0,
    error_rate REAL NOT NULL DEFAULT 0,
    product_name TEXT NOT NULL DEFAULT '',
    gpu_active_frac REAL NOT NULL DEFAULT 0,
    p_ngpus INTEGER NOT NULL DEFAULT 0,
    p_gpu_fpops REAL NOT NULL DEFAULT 0,
    misc TEXT NOT NULL DEFAULT ''
);
CREATE INDEX host_user ON host (userid);

CREATE TABLE host_app_version (
    host_id INTEGER NOT NULL,
    app_version_id INTEGER NOT NULL,
    pfc_n REAL NOT NULL DEFAULT 0,
    pfc_avg REAL NOT NULL DEFAULT 0,
    et_n REAL NOT NULL DEFAULT 0,
    et_avg REAL NOT NULL DEFAULT 0,
    et_var REAL NOT NULL DEFAULT 0,
    et_q REAL NOT NULL DEFAULT 0,
    max_jobs_per_day INTEGER NOT NULL DEFAULT 0,
    n_jobs_today INTEGER NOT NULL DEFAULT 0,
    turnaround_n REAL NOT NULL DEFAULT 0,
    turnaround_avg REAL NOT NULL DEFAULT 0,
    turnaround_var REAL NOT NULL DEFAULT 0,
    turnaround_q REAL NOT NULL DEFAULT 0,
    consecutive_valid INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (host_id, app_version_id)
) WITHOUT ROWID;

CREATE TABLE workunit (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    appid INTEGER NOT NULL,
    name TEXT NOT NULL,
    xml_doc TEXT NOT NULL,
    batch INTEGER NOT NULL DEFAULT 0,
    rsc_fpops_est REAL NOT NULL,
    rsc_fpops_bound REAL NOT NULL,
    rsc_memory_bound REAL NOT NULL,
    rsc_disk_bound REAL NOT NULL,
    need_validate INTEGER NOT NULL DEFAULT 0,
    canonical_resultid INTEGER NOT NULL DEFAULT 0,
    canonical_credit REAL NOT NULL DEFAULT 0,
    transition_time INTEGER NOT NULL,
    delay_bound INTEGER NOT NULL,
    error_mask INTEGER NOT NULL DEFAULT 0,
    file_delete_state INTEGER NOT NULL DEFAULT 0,
    assimilate_state INTEGER NOT NULL DEFAULT 0,
    opaque REAL NOT NULL DEFAULT 0,
    min_quorum INTEGER NOT NULL,
    target_nresults INTEGER NOT NULL,
    max_error_results INTEGER NOT NULL,
    max_total_results INTEGER NOT NULL,
    max_success_results INTEGER NOT NULL,
    -- cloud-boinc: the output template text itself, not a file name
    result_template TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    mod_time INTEGER NOT NULL DEFAULT 0,
    rsc_bandwidth_bound REAL NOT NULL DEFAULT 0,
    app_version_id INTEGER NOT NULL DEFAULT 0,
    transitioner_flags INTEGER NOT NULL DEFAULT 0,
    keywords TEXT NOT NULL DEFAULT '',
    app_version_num INTEGER NOT NULL DEFAULT 0,
    n_sent INTEGER NOT NULL DEFAULT 0,
    -- cloud-boinc: bumped by every transition; guards against two transitions
    -- of one workunit writing over each other
    rev INTEGER NOT NULL DEFAULT 0,
    -- cloud-boinc: random value written by a transition's workunit update; the
    -- other statements of its batch apply only while it is still there
    commit_token INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX wu_name ON workunit (name);
-- Workunits with nothing pending are parked at INT_MAX, as the transitioner does.
CREATE INDEX wu_transition ON workunit (transition_time) WHERE transition_time < 2147483647;
CREATE INDEX wu_assim ON workunit (appid, id) WHERE assimilate_state = 1;
CREATE INDEX wu_batch ON workunit (batch) WHERE batch > 0;

CREATE TABLE result (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    workunitid INTEGER NOT NULL,
    server_state INTEGER NOT NULL,
    outcome INTEGER NOT NULL DEFAULT 0,
    client_state INTEGER NOT NULL DEFAULT 0,
    hostid INTEGER NOT NULL DEFAULT 0,
    userid INTEGER NOT NULL DEFAULT 0,
    report_deadline INTEGER NOT NULL DEFAULT 0,
    sent_time INTEGER NOT NULL DEFAULT 0,
    received_time INTEGER NOT NULL DEFAULT 0,
    name TEXT NOT NULL,
    cpu_time REAL NOT NULL DEFAULT 0,
    xml_doc_in TEXT NOT NULL,
    xml_doc_out TEXT NOT NULL DEFAULT '',
    stderr_out TEXT NOT NULL DEFAULT '',
    batch INTEGER NOT NULL DEFAULT 0,
    file_delete_state INTEGER NOT NULL DEFAULT 0,
    validate_state INTEGER NOT NULL DEFAULT 0,
    claimed_credit REAL NOT NULL DEFAULT 0,
    granted_credit REAL NOT NULL DEFAULT 0,
    opaque REAL NOT NULL DEFAULT 0,
    random INTEGER NOT NULL DEFAULT 0,
    app_version_num INTEGER NOT NULL DEFAULT 0,
    appid INTEGER NOT NULL,
    exit_status INTEGER NOT NULL DEFAULT 0,
    teamid INTEGER NOT NULL DEFAULT 0,
    priority INTEGER NOT NULL DEFAULT 0,
    mod_time INTEGER NOT NULL DEFAULT 0,
    elapsed_time REAL NOT NULL DEFAULT 0,
    flops_estimate REAL NOT NULL DEFAULT 0,
    app_version_id INTEGER NOT NULL DEFAULT 0,
    runtime_outlier INTEGER NOT NULL DEFAULT 0,
    peak_working_set_size REAL NOT NULL DEFAULT 0,
    peak_swap_size REAL NOT NULL DEFAULT 0,
    peak_disk_usage REAL NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX res_name ON result (name);
CREATE INDEX res_wuid ON result (workunitid);
-- Dispatch: unsent results only (server_state 2).
CREATE INDEX res_unsent ON result (appid, priority DESC, id) WHERE server_state = 2;
-- In progress on a host (server_state 4): resend, per-host limits.
CREATE INDEX res_host_inprog ON result (hostid) WHERE server_state = 4;
-- Recent results of a host, for the tasks page.
CREATE INDEX res_host ON result (hostid, id) WHERE hostid > 0;

CREATE TABLE batch (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL DEFAULT 0,
    create_time INTEGER NOT NULL,
    name TEXT NOT NULL DEFAULT '',
    app_id INTEGER NOT NULL DEFAULT 0,
    state INTEGER NOT NULL DEFAULT 0,
    description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE assignment (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    target_id INTEGER NOT NULL,
    target_type INTEGER NOT NULL,
    multi INTEGER NOT NULL DEFAULT 0,
    workunitid INTEGER NOT NULL,
    resultid INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX asgn_target ON assignment (target_type, target_id);
CREATE INDEX asgn_wu ON assignment (workunitid);

-- Files staged in the bucket: app version files and job inputs.
CREATE TABLE job_file (
    name TEXT PRIMARY KEY,
    create_time INTEGER NOT NULL,
    nbytes INTEGER NOT NULL,
    md5 TEXT NOT NULL,
    -- 0: delete with the last workunit that uses it; 1: keep
    no_delete INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
