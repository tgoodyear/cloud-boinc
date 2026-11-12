#!/bin/sh
# Asks the project's web RPCs what a client or an account manager asks after an
# account has moved, and prints the replies with keys shown as fingerprints.
#   web-probe.sh --work DIR --base URL [--host-header NAME]
# --base is the master URL's address, for example http://127.0.0.1/proj/ (the stock
# server and the local cloud-boinc both answer there). The three accounts of the
# fixtures (secrets.json): the address in lower case with the right password, the
# same with a wrong one, and the third account, whose stock address has capitals.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" base="" hostname=project.test
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --base) base="$2" ;; --host-header) hostname="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$base" ] || { usage; }
WORK="$work"
. "$(dirname "$0")/lib.sh"
fp() { python3 -I "$HERE/fingerprint.py"; }
# get_fp URL: the reply with keys shown as fingerprints. A request that fails stops the script.
get_fp() {
    _body="$(curl -sS -H "Host: $hostname" "$1")" || die "request failed: $1"
    printf '%s\n' "$_body" | fp
}
echo "== get_project_config.php"
get_fp "${base}get_project_config.php"
for u in 1 2 3; do
    email="$(secret $u email | tr '[:upper:]' '[:lower:]')"
    good="$(secret $u passwd_md5)"
    echo "== user $u lookup_account.php, right password hash"
    get_fp "${base}lookup_account.php?email_addr=$email&passwd_hash=$good"
    echo "== user $u lookup_account.php, wrong password hash"
    get_fp "${base}lookup_account.php?email_addr=$email&passwd_hash=00000000000000000000000000000000"
    echo "== user $u lookup_account.php, no password hash"
    get_fp "${base}lookup_account.php?email_addr=$email"
    echo "== user $u am_get_info.php with the account key"
    get_fp "${base}am_get_info.php?account_key=$(secret $u authenticator)"
    # The site's login form: the status, where it sends the browser, and whether it sets the
    # sign-in cookie.
    for how in right wrong; do
        pw="$(secret $u password)"
        [ "$how" = right ] || pw="not-the-password"
        echo "== user $u login_action.php, $how password"
        _hdr="$(curl -sS -o /dev/null -D - -H "Host: $hostname" --data-urlencode "email_addr=$(secret $u email)" \
            --data-urlencode "passwd=$pw" "${base}login_action.php")" || die "login request failed"
        printf '%s\n' "$_hdr" | grep -iE '^(HTTP/|location:|set-cookie:)' | tr -d '\r' |
            sed -E 's/^[Ss]et-[Cc]ookie:.*/set-cookie: yes/'
    done
done
