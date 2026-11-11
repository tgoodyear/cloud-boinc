#!/bin/sh
# Sets up a test project on a deployment: settings, public keys, one platform,
# the test app from tests/app, an account, and jobs that upper-case a small
# input file. Safe to run again; later runs add jobs under a new prefix.
#   dev/seed.sh --url <base url> --token-file <file> --keys <dir> \
#       [--jobs <n>] [--name-prefix <s>] [--quorum <n>] [--email <addr>] \
#       [--platform <name>] [--app-binary <file>] [--cboinc <file>]
# With dev/local.sh, --token-file is <state>/token and --keys is <state>/keys.
# Prints the new account's authenticator as JSON at the end; an address that
# has an account already gets a note instead.
set -eu
cd "$(dirname "$0")/.."
url="" token="" keys="" jobs=3 prefix="seed_$(date +%s)" quorum=1
email="tester@example.org"
platform="x86_64-pc-linux-gnu"
app_binary="target/x86_64-unknown-linux-musl/release/cb-test-app"
cboinc="target/release/cboinc"
while [ $# -gt 0 ]; do
    case "$1" in
        --url) url="$2" ;;
        --token-file) token="$2" ;;
        --keys) keys="$2" ;;
        --jobs) jobs="$2" ;;
        --name-prefix) prefix="$2" ;;
        --quorum) quorum="$2" ;;
        --email) email="$2" ;;
        --platform) platform="$2" ;;
        --app-binary) app_binary="$2" ;;
        --cboinc) cboinc="$2" ;;
        *) sed -n '2,10p' "$0" >&2; exit 2 ;;
    esac
    shift 2
done
[ -n "$url" ] && [ -n "$token" ] && [ -n "$keys" ] || { sed -n '2,10p' "$0" >&2; exit 2; }
export CBOINC_URL="${url%/}" CBOINC_TOKEN_FILE="$token"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cat > "$work/config.xml" <<XML
<config>
<long_name>cloud-boinc test project</long_name>
<master_url>$CBOINC_URL/</master_url>
<daily_result_quota>100000</daily_result_quota>
<max_wus_to_send>8</max_wus_to_send>
<max_wus_in_progress>8</max_wus_in_progress>
<min_sendwork_interval>0</min_sendwork_interval>
</config>
XML
"$cboinc" config-put --name config --file "$work/config.xml"
"$cboinc" config-put --name code_sign_public_key --file "$keys/code_sign_public"
"$cboinc" config-put --name upload_public_key --file "$keys/upload_public"
"$cboinc" platform-put --name "$platform" --user-friendly-name "$platform"
"$cboinc" app-put --name upper --user-friendly-name "Upper-case test app"
if ! "$cboinc" catalog | grep -q '"version_num": *100'; then
    "$cboinc" version-add --app upper --platform "$platform" --version-num 100 \
        --key "$keys/code_sign_private" --main-program "$app_binary"
fi
printf 'hello from cloud-boinc\n' > "$work/seed_input.txt"
"$cboinc" file-put --file "$work/seed_input.txt" --no-delete
"$cboinc" submit --app upper --input-template dev/fixtures/in_template.xml \
    --result-template dev/fixtures/out_template.xml --count "$jobs" \
    --name-prefix "$prefix" --input-file seed_input.txt --quorum "$quorum"
"$cboinc" user-add --email "$email" --user-name tester ||
    echo "seed.sh: no account made; $email probably has one already" >&2
