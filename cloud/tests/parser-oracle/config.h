// The configuration header for the parser oracle.
//
// Upstream sources start with `#include "config.h"`, which `./configure`
// writes from `config.h.in` at the top of the tree. The oracle does not
// run the autotools build, so this file holds the values `./configure
// --enable-client` writes on x86-64 GNU/Linux with glibc, OpenSSL and
// libcurl, without the install paths. It affects only which system headers
// and library functions the compiled upstream code uses; it does not touch
// any protocol code.

#ifndef BOINC_CONFIG_H
#define BOINC_CONFIG_H

#include "version.h"

#define HOSTTYPE "x86_64-pc-linux-gnu"
#define HOSTTYPEALT "i686-pc-linux-gnu"
#define PACKAGE "boinc"
#define PACKAGE_NAME "BOINC"
#define PACKAGE_VERSION "8.3.0"
#define VERSION "8.3.0"
#define USE_OPENSSL 1
#define X_DISPLAY_MISSING 1
#define RETSIGTYPE void
#define STDC_HEADERS 1
#define TIME_WITH_SYS_TIME 1
#define LIBCURL_CABUNDLE "/etc/ssl/certs/ca-certificates.crt"
#define LIBCURL_FEATURE_SSL 1
#define LIBCURL_PROTOCOL_HTTP 1
#define LIBCURL_PROTOCOL_HTTPS 1

#define ASM_SUPPORTS_XGETBV 1
#define HAVE_DECL_CPUID 0
#define HAVE_DECL_FPRESET 0
#define HAVE_DECL_XGETBV 0
#define HAVE_DECL__CPUID 0
#define HAVE_DECL__FPRESET 0
#define HAVE_DECL__XGETBV 1
#define HAVE_DECL___CPUID 0
#define HAVE_DECL___XGETBV 0

#define HAVE_ARPA_INET_H 1
#define HAVE_DIRENT_H 1
#define HAVE_DLFCN_H 1
#define HAVE_EMMINTRIN_H 1
#define HAVE_ERRNO_H 1
#define HAVE_EXECINFO_H 1
#define HAVE_FCNTL_H 1
#define HAVE_FLOAT_H 1
#define HAVE_GRP_H 1
#define HAVE_IMMINTRIN_H 1
#define HAVE_INTTYPES_H 1
#define HAVE_NETDB_H 1
#define HAVE_NETINET_ETHER_H 1
#define HAVE_NETINET_IF_ETHER_H 1
#define HAVE_NETINET_IN_H 1
#define HAVE_NETINET_TCP_H 1
#define HAVE_NET_IF_ARP_H 1
#define HAVE_NET_IF_H 1
#define HAVE_PMMINTRIN_H 1
#define HAVE_SETJMP_H 1
#define HAVE_SIGNAL_H 1
#define HAVE_STDINT_H 1
#define HAVE_STDIO_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRINGS_H 1
#define HAVE_STRING_H 1
#define HAVE_SYS_AUXV_H 1
#define HAVE_SYS_FCNTL_H 1
#define HAVE_SYS_FILE_H 1
#define HAVE_SYS_IOCTL_H 1
#define HAVE_SYS_IPC_H 1
#define HAVE_SYS_MOUNT_H 1
#define HAVE_SYS_MSG_H 1
#define HAVE_SYS_PARAM_H 1
#define HAVE_SYS_RESOURCE_H 1
#define HAVE_SYS_SELECT_H 1
#define HAVE_SYS_SEM_H 1
#define HAVE_SYS_SHM_H 1
#define HAVE_SYS_SOCKET_H 1
#define HAVE_SYS_STATFS_H 1
#define HAVE_SYS_STATVFS_H 1
#define HAVE_SYS_STAT_H 1
#define HAVE_SYS_SWAP_H 1
#define HAVE_SYS_TIME_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_SYS_UN_H 1
#define HAVE_SYS_UTSNAME_H 1
#define HAVE_SYS_WAIT_H 1
#define HAVE_UNISTD_H 1
#define HAVE_UTMP_H 1
#define HAVE_X86INTRIN_H 1
#define HAVE_XMMINTRIN_H 1

#define HAVE_DAEMON 1
#define HAVE_ETHER_NTOA 1
#define HAVE_FTOK 1
#define HAVE_GETUTENT 1
#define HAVE_GMTIME 1
#define HAVE_GMTIME_R 1
#define HAVE_LIBCURL 1
#define HAVE_LOCALTIME 1
#define HAVE_LOCALTIME_R 1
#define HAVE_NAMESPACES 1
#define HAVE_PTHREAD 1
#define HAVE_PTHREAD_PRIO_INHERIT 1
#define HAVE_PUTENV 1
#define HAVE_RESOLV 1
#define HAVE_SCHED_SETSCHEDULER 1
#define HAVE_SETENV 1
#define HAVE_SETPRIORITY 1
#define HAVE_SETUTENT 1
#define HAVE_SIGACTION 1
#define HAVE_SOCKLEN_T 1
#define HAVE_STAT64 1
#define HAVE_STD_LOCALE 1
#define HAVE_STD_MAX 1
#define HAVE_STD_MIN 1
#define HAVE_STD_TRANSFORM 1
#define HAVE_STRCASECMP 1
#define HAVE_STRCASESTR 1
#define HAVE_STRDUP 1
#define HAVE_STRLCAT 1
#define HAVE_STRLCPY 1
#define HAVE_STRTOULL 1
#define HAVE_STRUCT_ETHER_ADDR 1
#define HAVE_STRUCT_IFCONF 1
#define HAVE_STRUCT_IFREQ 1
#define HAVE_STRUCT_TM_TM_ZONE 1
#define HAVE_TM_ZONE 1
#define HAVE_UNSETENV 1
#define HAVE_USELOCALE 1
#define HAVE_VPRINTF 1
#define HAVE__PROC_MEMINFO 1
#define HAVE__PROC_SELF_STAT 1

#include "project_specific_defines.h"

#endif
