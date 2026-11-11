//! State constants shared by the client, the scheduler and the database.
//!
//! Values are copied from `db/boinc_db_types.h` and `lib/common_defs.h`.

/// `result.server_state`: the workunit is not ready to be sent.
pub const RESULT_SERVER_STATE_INACTIVE: i32 = 1;
/// `result.server_state`: ready to send.
pub const RESULT_SERVER_STATE_UNSENT: i32 = 2;
/// `result.server_state`: sent to a host, no reply yet.
pub const RESULT_SERVER_STATE_IN_PROGRESS: i32 = 4;
/// `result.server_state`: finished, one way or another; see the outcome.
pub const RESULT_SERVER_STATE_OVER: i32 = 5;

/// `result.outcome`: not over yet.
pub const RESULT_OUTCOME_INIT: i32 = 0;
/// `result.outcome`: the client reported success.
pub const RESULT_OUTCOME_SUCCESS: i32 = 1;
/// `result.outcome`: the scheduler could not send it.
pub const RESULT_OUTCOME_COULDNT_SEND: i32 = 2;
/// `result.outcome`: download, compute or upload error on the client.
pub const RESULT_OUTCOME_CLIENT_ERROR: i32 = 3;
/// `result.outcome`: the deadline passed without a report.
pub const RESULT_OUTCOME_NO_REPLY: i32 = 4;
/// `result.outcome`: never sent because the workunit no longer needed it.
pub const RESULT_OUTCOME_DIDNT_NEED: i32 = 5;
/// `result.outcome`: the validator could not process the output.
pub const RESULT_OUTCOME_VALIDATE_ERROR: i32 = 6;
/// `result.outcome`: the client detached from the project.
pub const RESULT_OUTCOME_CLIENT_DETACHED: i32 = 7;

/// `result.validate_state`: not validated yet.
pub const VALIDATE_STATE_INIT: i32 = 0;
/// `result.validate_state`: valid.
pub const VALIDATE_STATE_VALID: i32 = 1;
/// `result.validate_state`: invalid.
pub const VALIDATE_STATE_INVALID: i32 = 2;
/// `result.validate_state`: no check was needed (for example the workunit
/// had an error).
pub const VALIDATE_STATE_NO_CHECK: i32 = 3;
/// `result.validate_state`: the comparison was inconclusive; try again.
pub const VALIDATE_STATE_INCONCLUSIVE: i32 = 4;
/// `result.validate_state`: reported after the canonical result's files
/// were deleted.
pub const VALIDATE_STATE_TOO_LATE: i32 = 5;

/// `workunit.assimilate_state`: not ready.
pub const ASSIMILATE_INIT: i32 = 0;
/// `workunit.assimilate_state`: ready to assimilate.
pub const ASSIMILATE_READY: i32 = 1;
/// `workunit.assimilate_state`: assimilated.
pub const ASSIMILATE_DONE: i32 = 2;

/// `file_delete_state`: not ready.
pub const FILE_DELETE_INIT: i32 = 0;
/// `file_delete_state`: ready to delete.
pub const FILE_DELETE_READY: i32 = 1;
/// `file_delete_state`: deleted.
pub const FILE_DELETE_DONE: i32 = 2;
/// `file_delete_state`: deletion failed.
pub const FILE_DELETE_ERROR: i32 = 3;

/// `workunit.error_mask` bit: a result could not be sent.
pub const WU_ERROR_COULDNT_SEND_RESULT: i32 = 1;
/// `workunit.error_mask` bit: too many error results.
pub const WU_ERROR_TOO_MANY_ERROR_RESULTS: i32 = 2;
/// `workunit.error_mask` bit: too many success results without consensus.
pub const WU_ERROR_TOO_MANY_SUCCESS_RESULTS: i32 = 4;
/// `workunit.error_mask` bit: too many results in total.
pub const WU_ERROR_TOO_MANY_TOTAL_RESULTS: i32 = 8;
/// `workunit.error_mask` bit: cancelled.
pub const WU_ERROR_CANCELLED: i32 = 16;
/// `workunit.error_mask` bit: the canonical result's output is gone.
pub const WU_ERROR_NO_CANONICAL_RESULT: i32 = 32;

/// `transitioner_flags`: the transitioner leaves this workunit alone.
pub const TRANSITION_NONE: i32 = 1;
/// `transitioner_flags`: no new results are created.
pub const TRANSITION_NO_NEW_RESULTS: i32 = 2;

/// Client result state (`<state>` in a reported result): new.
pub const RESULT_NEW: i32 = 0;
/// Client result state: input files are downloading.
pub const RESULT_FILES_DOWNLOADING: i32 = 1;
/// Client result state: input files are present; may be running.
pub const RESULT_FILES_DOWNLOADED: i32 = 2;
/// Client result state: the computation failed.
pub const RESULT_COMPUTE_ERROR: i32 = 3;
/// Client result state: output files are uploading.
pub const RESULT_FILES_UPLOADING: i32 = 4;
/// Client result state: output files are uploaded; this is success.
pub const RESULT_FILES_UPLOADED: i32 = 5;
/// Client result state: aborted by the user or the project.
pub const RESULT_ABORTED: i32 = 6;
/// Client result state: an output file upload failed for good.
pub const RESULT_UPLOAD_FAILED: i32 = 7;

/// Processor type numbers (`PROC_TYPE_*` in `lib/coproc.h`).
pub const PROC_TYPE_CPU: i32 = 0;
/// NVIDIA GPU.
pub const PROC_TYPE_NVIDIA_GPU: i32 = 1;
/// AMD GPU.
pub const PROC_TYPE_AMD_GPU: i32 = 2;
/// Intel GPU.
pub const PROC_TYPE_INTEL_GPU: i32 = 3;
/// Apple GPU.
pub const PROC_TYPE_APPLE_GPU: i32 = 4;

/// `DOCKER_TYPE` in `lib/common_defs.h`: no container runtime.
pub const DOCKER_TYPE_NONE: i32 = 0;
/// Docker.
pub const DOCKER_TYPE_DOCKER: i32 = 1;
/// Podman.
pub const DOCKER_TYPE_PODMAN: i32 = 2;
/// WSL containers.
pub const DOCKER_TYPE_WSLC: i32 = 3;

/// Upper bound on `<request_delay>` (`DELAY_MAX` in `sched/sched_main.h`):
/// two days.
pub const DELAY_MAX: f64 = 2.0 * 86400.0;
